// Copyright 2019 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

//! Enables pre-boot setup, instantiation and booting of a Firecracker VMM.

use timerfd::{ClockId, SetTimeFlags, TimerFd, TimerState};

use std::fmt::{Display, Formatter};
use std::fs::OpenOptions;
use std::time::Duration;

use super::{EpollContext, EpollDispatch, VcpuConfig, Vmm};

use controller::UserResult;
use controller::VmmActionError;
use device_manager;
#[cfg(target_arch = "x86_64")]
use device_manager::legacy::PortIODeviceManager;
use device_manager::mmio::MMIODeviceManager;
use devices::virtio::vsock::{TYPE_VSOCK, VSOCK_EVENTS_COUNT};
use devices::virtio::{MmioDevice, BLOCK_EVENTS_COUNT, NET_EVENTS_COUNT, TYPE_BLOCK, TYPE_NET};
use error::*;
use logger::{Metric, LOGGER, METRICS};
use memory_model::{GuestAddress, GuestMemory, GuestMemoryError};
use polly::event_manager::EventManager;
use resources::VmResources;
use utils::time::TimestampUs;
use vmm_config;
use vmm_config::boot_source::BootConfig;
use vstate::{self, KvmContext, Vm};

const WRITE_METRICS_PERIOD_SECONDS: u64 = 60;

/// Errors associated with starting the instance.
// TODO: add error kind to these variants because not all these errors are user or internal.
#[derive(Debug)]
pub enum StartMicrovmError {
    /// Cannot configure the VM.
    ConfigureVm(vstate::Error),
    /// Unable to seek the block device backing file due to invalid permissions or
    /// the file was deleted/corrupted.
    CreateBlockDevice(std::io::Error),
    /// Split this at some point.
    /// Internal errors are due to resource exhaustion.
    /// Users errors are due to invalid permissions.
    CreateNetDevice(devices::virtio::Error),
    /// Failed to create a `RateLimiter` object.
    CreateRateLimiter(std::io::Error),
    /// Failed to create the backend for the vsock device.
    CreateVsockBackend(devices::virtio::vsock::VsockUnixBackendError),
    /// Failed to create the vsock device.
    CreateVsockDevice(devices::virtio::vsock::VsockError),
    /// Memory regions are overlapping or mmap fails.
    GuestMemory(GuestMemoryError),
    // Temporarily added for mixing calls that may return an Error with others that may return a
    // StartMicrovmError within the same function.
    /// Internal error encountered while starting a microVM.
    Internal(Error),
    /// The kernel command line is invalid.
    KernelCmdline(String),
    /// Cannot load kernel due to invalid memory configuration or invalid kernel image.
    KernelLoader(kernel::loader::Error),
    /// Cannot load command line string.
    LoadCommandline(kernel::cmdline::Error),
    /// The start command was issued more than once.
    MicroVMAlreadyRunning,
    /// Cannot start the VM because the kernel was not configured.
    MissingKernelConfig,
    /// The net device configuration is missing the tap device.
    NetDeviceNotConfigured,
    /// Cannot open the block device backing file.
    OpenBlockDevice(std::io::Error),
    /// Cannot initialize a MMIO Block Device or add a device to the MMIO Bus.
    RegisterBlockDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Network Device or add a device to the MMIO Bus.
    RegisterNetDevice(device_manager::mmio::Error),
    /// Cannot initialize a MMIO Vsock Device or add a device to the MMIO Bus.
    RegisterVsockDevice(device_manager::mmio::Error),
}

/// It's convenient to automatically convert `kernel::cmdline::Error`s
/// to `StartMicrovmError`s.
impl std::convert::From<kernel::cmdline::Error> for StartMicrovmError {
    fn from(e: kernel::cmdline::Error) -> StartMicrovmError {
        StartMicrovmError::KernelCmdline(e.to_string())
    }
}

impl Display for StartMicrovmError {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        use self::StartMicrovmError::*;
        match *self {
            ConfigureVm(ref err) => {
                let mut err_msg = format!("{:?}", err);
                err_msg = err_msg.replace("\"", "");

                write!(f, "Cannot configure virtual machine. {}", err_msg)
            }
            CreateBlockDevice(ref err) => write!(
                f,
                "Unable to seek the block device backing file due to invalid permissions or \
                 the file was deleted/corrupted. Error number: {}",
                err
            ),
            CreateRateLimiter(ref err) => write!(f, "Cannot create RateLimiter: {}", err),
            CreateVsockBackend(ref err) => {
                write!(f, "Cannot create backend for vsock device: {:?}", err)
            }
            CreateVsockDevice(ref err) => write!(f, "Cannot create vsock device: {:?}", err),
            CreateNetDevice(ref err) => {
                let mut err_msg = format!("{:?}", err);
                err_msg = err_msg.replace("\"", "");

                write!(f, "Cannot create network device. {}", err_msg)
            }
            GuestMemory(ref err) => {
                // Remove imbricated quotes from error message.
                let mut err_msg = format!("{:?}", err);
                err_msg = err_msg.replace("\"", "");
                write!(f, "Invalid Memory Configuration: {}", err_msg)
            }
            Internal(ref err) => write!(f, "Internal error while starting microVM: {:?}", err),
            KernelCmdline(ref err) => write!(f, "Invalid kernel command line: {}", err),
            KernelLoader(ref err) => {
                let mut err_msg = format!("{}", err);
                err_msg = err_msg.replace("\"", "");
                write!(
                    f,
                    "Cannot load kernel due to invalid memory configuration or invalid kernel \
                     image. {}",
                    err_msg
                )
            }
            LoadCommandline(ref err) => {
                let mut err_msg = format!("{}", err);
                err_msg = err_msg.replace("\"", "");
                write!(f, "Cannot load command line string. {}", err_msg)
            }
            MicroVMAlreadyRunning => write!(f, "Microvm already running."),
            MissingKernelConfig => write!(f, "Cannot start microvm without kernel configuration."),
            NetDeviceNotConfigured => {
                write!(f, "The net device configuration is missing the tap device.")
            }
            OpenBlockDevice(ref err) => {
                let mut err_msg = format!("{:?}", err);
                err_msg = err_msg.replace("\"", "");

                write!(f, "Cannot open the block device backing file. {}", err_msg)
            }
            RegisterBlockDevice(ref err) => {
                let mut err_msg = format!("{}", err);
                err_msg = err_msg.replace("\"", "");
                write!(
                    f,
                    "Cannot initialize a MMIO Block Device or add a device to the MMIO Bus. {}",
                    err_msg
                )
            }
            RegisterNetDevice(ref err) => {
                let mut err_msg = format!("{}", err);
                err_msg = err_msg.replace("\"", "");

                write!(
                    f,
                    "Cannot initialize a MMIO Network Device or add a device to the MMIO Bus. {}",
                    err_msg
                )
            }
            RegisterVsockDevice(ref err) => {
                let mut err_msg = format!("{}", err);
                err_msg = err_msg.replace("\"", "");

                write!(
                    f,
                    "Cannot initialize a MMIO Vsock Device or add a device to the MMIO Bus. {}",
                    err_msg
                )
            }
        }
    }
}

/// Builds and starts a microVM based on the current configuration.
pub fn build_microvm(
    vm_resources: &VmResources,
    epoll_context: &mut EpollContext,
    seccomp_level: u32,
) -> std::result::Result<Vmm, VmmActionError> {
    let boot_config = vm_resources
        .boot_source()
        .ok_or(StartMicrovmError::MissingKernelConfig)?;

    // Timestamp for measuring microVM boot duration.
    let request_ts = TimestampUs::default();

    let guest_memory = create_guest_memory(vm_resources)?;
    let vcpu_config = vcpu_config(vm_resources);
    let entry_addr = load_kernel(boot_config, &guest_memory)?;
    // Clone the command-line so that a failed boot doesn't pollute the original.
    let kernel_cmdline = boot_config.cmdline.clone();
    let write_metrics_event_fd = setup_metrics(epoll_context)?;
    let event_manager = setup_event_manager(epoll_context)?;
    let vm = setup_kvm_vm(guest_memory.clone())?;

    // Instantiate the MMIO device manager.
    // 'mmio_base' address has to be an address which is protected by the kernel
    // and is architectural specific.
    let mmio_device_manager = MMIODeviceManager::new(
        guest_memory.clone(),
        &mut (arch::MMIO_MEM_START as u64),
        (arch::IRQ_BASE, arch::IRQ_MAX),
    );

    // TODO: All Vmm setup should move outside of Vmm, including irqchip and legacy devices setup.
    // TODO: The Vmm would be created as the last step that brings all the configured resources
    // TODO: together.
    let mut vmm = Vmm {
        stdin_handle: std::io::stdin(),
        guest_memory,
        vcpu_config,
        kernel_cmdline,
        vcpus_handles: Vec::new(),
        exit_evt: None,
        vm,
        mmio_device_manager,
        #[cfg(target_arch = "x86_64")]
        pio_device_manager: PortIODeviceManager::new()
            .map_err(Error::CreateLegacyDevice)
            .map_err(StartMicrovmError::Internal)?,
        write_metrics_event_fd,
        seccomp_level,
        event_manager,
    };

    // For x86_64 we need to create the interrupt controller before calling `KVM_CREATE_VCPUS`
    // while on aarch64 we need to do it the other way around.
    #[cfg(target_arch = "x86_64")]
    {
        vmm.setup_interrupt_controller()
            .map_err(StartMicrovmError::Internal)?;
        // This call has to be here after setting up the irqchip, because
        // we set up some irqfd inside for some reason.
        vmm.attach_legacy_devices()
            .map_err(StartMicrovmError::Internal)?;
    }

    let vcpus = vmm
        .create_vcpus(entry_addr, request_ts)
        .map_err(StartMicrovmError::Internal)?;

    #[cfg(target_arch = "aarch64")]
    {
        vmm.setup_interrupt_controller()
            .map_err(StartMicrovmError::Internal)?;
        vmm.attach_legacy_devices()
            .map_err(StartMicrovmError::Internal)?;
    }

    attach_block_devices(&mut vmm, vm_resources, epoll_context)?;
    attach_net_devices(&mut vmm, vm_resources, epoll_context)?;
    attach_vsock_device(&mut vmm, vm_resources, epoll_context)?;

    // Write the kernel command line to guest memory. This is x86_64 specific, since on
    // aarch64 the command line will be specified through the FDT.
    #[cfg(target_arch = "x86_64")]
    load_cmdline(&vmm)?;

    vmm.configure_system(vcpus.as_slice())
        .map_err(StartMicrovmError::Internal)?;
    vmm.register_events(epoll_context)
        .map_err(StartMicrovmError::Internal)?;
    vmm.start_vcpus(vcpus)
        .map_err(StartMicrovmError::Internal)?;

    arm_logger_and_metrics(&mut vmm);

    Ok(vmm)
}

fn create_guest_memory(
    vm_resources: &VmResources,
) -> std::result::Result<GuestMemory, StartMicrovmError> {
    let mem_size = vm_resources
        .vm_config()
        .mem_size_mib
        .ok_or(StartMicrovmError::GuestMemory(
            memory_model::GuestMemoryError::MemoryNotInitialized,
        ))?
        << 20;
    let arch_mem_regions = arch::arch_memory_regions(mem_size);

    Ok(GuestMemory::new(&arch_mem_regions).map_err(StartMicrovmError::GuestMemory)?)
}

fn vcpu_config(vm_resources: &VmResources) -> VcpuConfig {
    // The unwraps are ok to use because the values are initialized using defaults if not
    // supplied by the user.
    VcpuConfig {
        vcpu_count: vm_resources.vm_config().vcpu_count.unwrap(),
        ht_enabled: vm_resources.vm_config().ht_enabled.unwrap(),
        cpu_template: vm_resources.vm_config().cpu_template,
    }
}

fn load_kernel(
    boot_config: &BootConfig,
    guest_memory: &GuestMemory,
) -> std::result::Result<GuestAddress, StartMicrovmError> {
    let mut kernel_file = boot_config
        .kernel_file
        .try_clone()
        .map_err(|e| StartMicrovmError::Internal(Error::KernelFile(e)))?;

    let entry_addr =
        kernel::loader::load_kernel(guest_memory, &mut kernel_file, arch::get_kernel_start())
            .map_err(StartMicrovmError::KernelLoader)?;

    Ok(entry_addr)
}

#[cfg(target_arch = "x86_64")]
fn load_cmdline(vmm: &Vmm) -> std::result::Result<(), StartMicrovmError> {
    kernel::loader::load_cmdline(
        vmm.guest_memory(),
        GuestAddress(arch::x86_64::layout::CMDLINE_START),
        &vmm.kernel_cmdline
            .as_cstring()
            .map_err(StartMicrovmError::LoadCommandline)?,
    )
    .map_err(StartMicrovmError::LoadCommandline)
}

fn setup_metrics(
    epoll_context: &mut EpollContext,
) -> std::result::Result<TimerFd, StartMicrovmError> {
    let write_metrics_event_fd = TimerFd::new_custom(ClockId::Monotonic, true, true)
        .map_err(Error::TimerFd)
        .map_err(StartMicrovmError::Internal)?;
    // TODO: remove expect.
    epoll_context
        .add_epollin_event(
            // non-blocking & close on exec
            &write_metrics_event_fd,
            EpollDispatch::WriteMetrics,
        )
        .expect("Cannot add write metrics TimerFd to epoll.");
    Ok(write_metrics_event_fd)
}

fn setup_event_manager(
    epoll_context: &mut EpollContext,
) -> std::result::Result<EventManager, VmmActionError> {
    let event_manager = EventManager::new()
        .map_err(Error::EventManager)
        .map_err(StartMicrovmError::Internal)?;
    // TODO: remove expect.
    epoll_context
        .add_epollin_event(&event_manager, EpollDispatch::PollyEvent)
        .expect("Cannot cascade EventManager from epoll_context");
    Ok(event_manager)
}

fn setup_kvm_vm(guest_memory: GuestMemory) -> std::result::Result<Vm, VmmActionError> {
    let kvm = KvmContext::new()
        .map_err(Error::KvmContext)
        .map_err(StartMicrovmError::Internal)?;
    let mut vm = Vm::new(kvm.fd())
        .map_err(Error::Vm)
        .map_err(StartMicrovmError::Internal)?;
    vm.memory_init(guest_memory.clone(), kvm.max_memslots())
        .map_err(StartMicrovmError::ConfigureVm)?;
    Ok(vm)
}

/// Adds a MmioDevice.
fn attach_mmio_device(
    vmm: &mut Vmm,
    id: String,
    device: MmioDevice,
) -> std::result::Result<(), StartMicrovmError> {
    // TODO: we currently map into StartMicrovmError::RegisterBlockDevice for all
    // devices at the end of device_manager.register_mmio_device.
    let type_id = device.device().device_type();
    let cmdline = &mut vmm.kernel_cmdline;

    vmm.mmio_device_manager
        .register_mmio_device(vmm.vm.fd(), device, cmdline, type_id, id.as_str())
        .map_err(StartMicrovmError::RegisterBlockDevice)?;

    Ok(())
}

fn attach_block_devices(
    vmm: &mut Vmm,
    vm_resources: &VmResources,
    epoll_context: &mut EpollContext,
) -> std::result::Result<(), StartMicrovmError> {
    use self::StartMicrovmError::*;

    // If no PARTUUID was specified for the root device, try with the /dev/vda.
    if vm_resources.block.has_root_block_device() && !vm_resources.block.has_partuuid_root() {
        let kernel_cmdline = &mut vmm.kernel_cmdline;

        kernel_cmdline.insert_str("root=/dev/vda")?;

        let flags = if vm_resources.block.has_read_only_root() {
            "ro"
        } else {
            "rw"
        };

        kernel_cmdline.insert_str(flags)?;
    }

    for drive_config in vm_resources.block.config_list.iter() {
        // Add the block device from file.
        let block_file = OpenOptions::new()
            .read(true)
            .write(!drive_config.is_read_only)
            .open(&drive_config.path_on_host)
            .map_err(OpenBlockDevice)?;

        if drive_config.is_root_device && drive_config.get_partuuid().is_some() {
            let kernel_cmdline = &mut vmm.kernel_cmdline;

            kernel_cmdline.insert_str(format!(
                "root=PARTUUID={}",
                //The unwrap is safe as we are firstly checking that partuuid is_some().
                drive_config.get_partuuid().unwrap()
            ))?;

            let flags = if drive_config.is_read_only() {
                "ro"
            } else {
                "rw"
            };

            kernel_cmdline.insert_str(flags)?;
        }

        let epoll_config = epoll_context.allocate_tokens_for_virtio_device(
            TYPE_BLOCK,
            &drive_config.drive_id,
            BLOCK_EVENTS_COUNT,
        );

        let rate_limiter = drive_config
            .rate_limiter
            .map(vmm_config::RateLimiterConfig::into_rate_limiter)
            .transpose()
            .map_err(CreateRateLimiter)?;

        let block_box = Box::new(
            devices::virtio::Block::new(
                block_file,
                drive_config.is_read_only,
                epoll_config,
                rate_limiter,
            )
            .map_err(CreateBlockDevice)?,
        );

        attach_mmio_device(
            vmm,
            drive_config.drive_id.clone(),
            MmioDevice::new(vmm.guest_memory().clone(), block_box).map_err(|e| {
                RegisterBlockDevice(super::device_manager::mmio::Error::CreateMmioDevice(e))
            })?,
        )?;
    }

    Ok(())
}

fn attach_net_devices(
    vmm: &mut Vmm,
    vm_resources: &VmResources,
    epoll_context: &mut EpollContext,
) -> UserResult {
    use self::StartMicrovmError::*;

    for cfg in vm_resources.network_interface.iter() {
        let epoll_config = epoll_context.allocate_tokens_for_virtio_device(
            TYPE_NET,
            &cfg.iface_id,
            NET_EVENTS_COUNT,
        );

        let allow_mmds_requests = cfg.allow_mmds_requests();

        let rx_rate_limiter = cfg
            .rx_rate_limiter
            .map(vmm_config::RateLimiterConfig::into_rate_limiter)
            .transpose()
            .map_err(CreateRateLimiter)?;

        let tx_rate_limiter = cfg
            .tx_rate_limiter
            .map(vmm_config::RateLimiterConfig::into_rate_limiter)
            .transpose()
            .map_err(CreateRateLimiter)?;

        let tap = cfg.open_tap().map_err(|_| NetDeviceNotConfigured)?;

        let net_box = Box::new(
            devices::virtio::Net::new_with_tap(
                tap,
                cfg.guest_mac(),
                epoll_config,
                rx_rate_limiter,
                tx_rate_limiter,
                allow_mmds_requests,
            )
            .map_err(CreateNetDevice)?,
        );

        attach_mmio_device(
            vmm,
            cfg.iface_id.clone(),
            MmioDevice::new(vmm.guest_memory().clone(), net_box).map_err(|e| {
                RegisterNetDevice(super::device_manager::mmio::Error::CreateMmioDevice(e))
            })?,
        )?;
    }

    Ok(())
}

fn attach_vsock_device(
    vmm: &mut Vmm,
    vm_resources: &VmResources,
    epoll_context: &mut EpollContext,
) -> UserResult {
    if let Some(cfg) = vm_resources.vsock.as_ref() {
        let backend = devices::virtio::vsock::VsockUnixBackend::new(
            u64::from(cfg.guest_cid),
            cfg.uds_path.clone(),
        )
        .map_err(StartMicrovmError::CreateVsockBackend)?;

        let epoll_config = epoll_context.allocate_tokens_for_virtio_device(
            TYPE_VSOCK,
            &cfg.vsock_id,
            VSOCK_EVENTS_COUNT,
        );

        let vsock_box = Box::new(
            devices::virtio::Vsock::new(u64::from(cfg.guest_cid), epoll_config, backend)
                .map_err(StartMicrovmError::CreateVsockDevice)?,
        );

        attach_mmio_device(
            vmm,
            cfg.vsock_id.clone(),
            MmioDevice::new(vmm.guest_memory().clone(), vsock_box).map_err(|e| {
                StartMicrovmError::RegisterVsockDevice(
                    super::device_manager::mmio::Error::CreateMmioDevice(e),
                )
            })?,
        )?;
    }

    Ok(())
}

fn arm_logger_and_metrics(vmm: &mut Vmm) {
    // Arm the log write timer.
    let timer_state = TimerState::Periodic {
        current: Duration::from_secs(WRITE_METRICS_PERIOD_SECONDS),
        interval: Duration::from_secs(WRITE_METRICS_PERIOD_SECONDS),
    };
    vmm.write_metrics_event_fd
        .set_state(timer_state, SetTimeFlags::Default);

    // Log the metrics straight away to check the process startup time.
    if LOGGER.log_metrics().is_err() {
        METRICS.logger.missed_metrics_count.inc();
    }
}