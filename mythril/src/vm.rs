use crate::boot_info::BootInfo;
use crate::device::{
    DeviceMap, MemReadRequest, MemWriteRequest, Port, PortReadRequest,
    PortWriteRequest,
};
use crate::error::{Error, Result};
use crate::memory::{
    self, GuestAddressSpace, GuestPhysAddr, HostPhysAddr, HostPhysFrame,
    Raw4kPage,
};
use crate::vcpu;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::RwLock;

pub static mut VM_MAP: Option<BTreeMap<usize, Arc<RwLock<VirtualMachine>>>> =
    None;

/// A configuration for a `VirtualMachine`
pub struct VirtualMachineConfig {
    _cpus: Vec<u8>,
    images: Vec<(String, GuestPhysAddr)>,
    bios: Option<String>,
    devices: DeviceMap,
    memory: u64, // in MB
}

impl VirtualMachineConfig {
    /// Creates a new configuration for a `VirtualMachine`
    ///
    /// # Arguments
    ///
    /// * `cpus` - A list of the cores used by the VM (by APIC id)
    /// * `memory` - The amount of VM memory (in MB)
    pub fn new(cpus: Vec<u8>, memory: u64) -> VirtualMachineConfig {
        VirtualMachineConfig {
            _cpus: cpus,
            images: vec![],
            devices: DeviceMap::default(),
            bios: None,
            memory: memory,
        }
    }

    /// Specify that the given image 'path' should be mapped to the given address
    ///
    /// The precise meaning of `image` will vary by platform. This will be a
    /// value suitable to be passed to `VmServices::read_file`.
    pub fn map_image(
        &mut self,
        image: String,
        addr: GuestPhysAddr,
    ) -> Result<()> {
        self.images.push((image, addr));
        Ok(())
    }

    /// Specify that the given image 'path' should be mapped as the BIOS
    ///
    /// The precise meaning of `image` will vary by platform. This will be a
    /// value suitable to be passed to `VmServices::read_file`.
    ///
    /// The BIOS image will be mapped such that the end of the image is at
    /// 0xffffffff and 0xfffff (i.e., it will be mapped in two places)
    pub fn map_bios(&mut self, bios: String) -> Result<()> {
        self.bios = Some(bios);
        Ok(())
    }

    /// Access the configurations `DeviceMap`
    pub fn device_map(&mut self) -> &mut DeviceMap {
        &mut self.devices
    }
}

/// A virtual machine
pub struct VirtualMachine {
    /// The configuration for this virtual machine (including the `DeviceMap`)
    pub config: VirtualMachineConfig,

    /// The guest virtual address space
    ///
    /// This will be shared by all `VCpu`s associated with this VM.
    pub guest_space: GuestAddressSpace,
}

impl VirtualMachine {
    /// Construct a new `VirtualMachine` using the given config
    ///
    /// This creates the guest address space (allocating the needed memory),
    /// and maps in the requested images.
    pub fn new(
        config: VirtualMachineConfig,
        info: &BootInfo,
    ) -> Result<Arc<RwLock<Self>>> {
        let guest_space = Self::setup_ept(&config, info)?;
        Ok(Arc::new(RwLock::new(Self {
            config: config,
            guest_space: guest_space,
        })))
    }

    pub fn on_mem_read(
        &mut self,
        vcpu: &vcpu::VCpu,
        addr: GuestPhysAddr,
        val: MemReadRequest,
    ) -> Result<()> {
        let dev =
            self.config
                .device_map()
                .device_for_mut(addr)
                .ok_or_else(|| {
                    Error::MissingDevice(format!(
                        "No device for address {:?}",
                        addr
                    ))
                })?;
        let view = memory::GuestAddressSpaceViewMut::from_vmcs(
            &vcpu.vmcs,
            &mut self.guest_space,
        )?;
        dev.on_mem_read(addr, val, view)
    }

    pub fn on_mem_write(
        &mut self,
        vcpu: &vcpu::VCpu,
        addr: GuestPhysAddr,
        val: MemWriteRequest,
    ) -> Result<()> {
        let dev =
            self.config
                .device_map()
                .device_for_mut(addr)
                .ok_or_else(|| {
                    Error::MissingDevice(format!(
                        "No device for address {:?}",
                        addr
                    ))
                })?;
        let view = memory::GuestAddressSpaceViewMut::from_vmcs(
            &vcpu.vmcs,
            &mut self.guest_space,
        )?;
        dev.on_mem_write(addr, val, view)
    }

    pub fn on_port_read(
        &mut self,
        vcpu: &vcpu::VCpu,
        port: Port,
        val: PortReadRequest,
    ) -> Result<()> {
        let dev =
            self.config
                .device_map()
                .device_for_mut(port)
                .ok_or_else(|| {
                    Error::MissingDevice(format!("No device for port {}", port))
                })?;
        let view = memory::GuestAddressSpaceViewMut::from_vmcs(
            &vcpu.vmcs,
            &mut self.guest_space,
        )?;
        dev.on_port_read(port, val, view)
    }

    pub fn on_port_write(
        &mut self,
        vcpu: &vcpu::VCpu,
        port: Port,
        val: PortWriteRequest,
    ) -> Result<()> {
        let dev =
            self.config
                .device_map()
                .device_for_mut(port)
                .ok_or_else(|| {
                    Error::MissingDevice(format!("No device for port {}", port))
                })?;
        let view = memory::GuestAddressSpaceViewMut::from_vmcs(
            &vcpu.vmcs,
            &mut self.guest_space,
        )?;
        dev.on_port_write(port, val, view)
    }

    fn map_data(
        image: &[u8],
        addr: &GuestPhysAddr,
        space: &mut GuestAddressSpace,
    ) -> Result<()> {
        for (i, chunk) in image.chunks(4096 as usize).enumerate() {
            let frame_ptr =
                Box::into_raw(Box::new(Raw4kPage::default())) as *mut u8;
            let frame = HostPhysFrame::from_start_address(HostPhysAddr::new(
                frame_ptr as u64,
            ))?;
            let chunk_ptr = chunk.as_ptr();
            unsafe {
                core::ptr::copy_nonoverlapping(
                    chunk_ptr,
                    frame_ptr,
                    chunk.len(),
                );
            }

            space.map_frame(
                memory::GuestPhysAddr::new(
                    addr.as_u64() + (i as u64 * 4096) as u64,
                ),
                frame,
                false,
            )?;
        }
        Ok(())
    }

    fn map_image(
        image: &str,
        addr: &GuestPhysAddr,
        space: &mut GuestAddressSpace,
        info: &BootInfo,
    ) -> Result<()> {
        let data = info
            .find_module(image)
            .ok_or_else(|| {
                Error::InvalidValue(format!("No such module '{}'", image))
            })?
            .data();
        Self::map_data(data, addr, space)
    }

    fn map_bios(
        bios: &str,
        space: &mut GuestAddressSpace,
        info: &BootInfo,
    ) -> Result<()> {
        let data = info
            .find_module(bios)
            .ok_or_else(|| {
                Error::InvalidValue(format!("No such bios '{}'", bios))
            })?
            .data();
        let bios_size = data.len() as u64;
        Self::map_data(
            data,
            &memory::GuestPhysAddr::new((1024 * 1024) - bios_size),
            space,
        )?;
        Self::map_data(
            data,
            &memory::GuestPhysAddr::new((4 * 1024 * 1024 * 1024) - bios_size),
            space,
        )
    }

    fn setup_ept(
        config: &VirtualMachineConfig,
        info: &BootInfo,
    ) -> Result<GuestAddressSpace> {
        let mut guest_space = GuestAddressSpace::new()?;

        // First map the bios
        if let Some(ref bios) = config.bios {
            Self::map_bios(&bios, &mut guest_space, info)?;
        }

        // Now map any guest iamges
        for image in config.images.iter() {
            Self::map_image(&image.0, &image.1, &mut guest_space, info)?;
        }

        // Iterate over each page
        for i in 0..(config.memory << 8) {
            match guest_space.map_new_frame(
                memory::GuestPhysAddr::new((i as u64 * 4096) as u64),
                false,
            ) {
                Ok(_) | Err(Error::DuplicateMapping(_)) => continue,
                Err(e) => return Err(e),
            }
        }

        Ok(guest_space)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_vm_creation() {
        let info = BootInfo::default();

        let config = VirtualMachineConfig::new(vec![1], 0);
        VirtualMachine::new(config, &info).unwrap();
    }
}
