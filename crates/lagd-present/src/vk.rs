//! The slice of Vulkan this layer needs, declared by hand.
//!
//! Pulling in `ash` would mean tracking a header version for no benefit: a
//! layer never constructs a Vulkan struct, it only walks `pNext` chains (whose
//! first two fields are the same in every struct) and passes pointers down
//! unchanged. The loader's own `VkLayer*CreateInfo` types are not in `ash`
//! anyway — they are loader-internal, so they would have to be declared here
//! regardless.
//!
//! Names here match the Vulkan headers exactly, including their casing. Anyone
//! checking this against the specification should be able to grep for the
//! spelling the specification uses.
#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_void};

// Dispatchable Vulkan handles are pointers to loader-internal objects. We only
// ever compare and forward them, never dereference them.
pub type VkInstance = *mut c_void;
pub type VkPhysicalDevice = *mut c_void;
pub type VkDevice = *mut c_void;
pub type VkQueue = *mut c_void;
pub type VkResult = i32;

pub const VK_SUCCESS: VkResult = 0;
pub const VK_ERROR_INITIALIZATION_FAILED: VkResult = -3;

pub type PFN_vkVoidFunction = Option<unsafe extern "system" fn()>;
pub type PFN_vkGetInstanceProcAddr =
    unsafe extern "system" fn(VkInstance, *const c_char) -> PFN_vkVoidFunction;
pub type PFN_vkGetDeviceProcAddr =
    unsafe extern "system" fn(VkDevice, *const c_char) -> PFN_vkVoidFunction;
pub type PFN_vkCreateInstance =
    unsafe extern "system" fn(*const VkBaseInStructure, *const c_void, *mut VkInstance) -> VkResult;
pub type PFN_vkDestroyInstance = unsafe extern "system" fn(VkInstance, *const c_void);
pub type PFN_vkCreateDevice = unsafe extern "system" fn(
    VkPhysicalDevice,
    *const VkBaseInStructure,
    *const c_void,
    *mut VkDevice,
) -> VkResult;
pub type PFN_vkDestroyDevice = unsafe extern "system" fn(VkDevice, *const c_void);
pub type PFN_vkQueuePresentKHR = unsafe extern "system" fn(VkQueue, *const c_void) -> VkResult;
pub type PFN_vkGetDeviceQueue = unsafe extern "system" fn(VkDevice, u32, u32, *mut VkQueue);
pub type PFN_vkGetDeviceQueue2 = unsafe extern "system" fn(VkDevice, *const c_void, *mut VkQueue);

/// Every Vulkan struct starts with these two fields, which is what makes
/// walking an arbitrary `pNext` chain legal.
#[repr(C)]
pub struct VkBaseInStructure {
    pub s_type: i32,
    pub p_next: *const VkBaseInStructure,
}

pub const VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO: i32 = 47;
pub const VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO: i32 = 48;

/// `VkLayerFunction::VK_LAYER_LINK_INFO`. The loader puts several kinds of
/// struct in the chain under the same `sType`, distinguished by this field.
pub const VK_LAYER_LINK_INFO: i32 = 0;

pub const LAYER_NEGOTIATE_INTERFACE_STRUCT: i32 = 1;

#[repr(C)]
pub struct VkLayerInstanceLink {
    pub p_next: *mut VkLayerInstanceLink,
    pub pfn_next_get_instance_proc_addr: PFN_vkGetInstanceProcAddr,
    pub pfn_next_get_physical_device_proc_addr: *mut c_void,
}

#[repr(C)]
pub union VkLayerInstanceCreateInfoUnion {
    pub p_layer_info: *mut VkLayerInstanceLink,
    pub pfn_set_instance_loader_data: *mut c_void,
    pub layer_device: *mut c_void,
    pub loader_features: u32,
}

#[repr(C)]
pub struct VkLayerInstanceCreateInfo {
    pub s_type: i32,
    pub p_next: *const VkBaseInStructure,
    pub function: i32,
    pub u: VkLayerInstanceCreateInfoUnion,
}

#[repr(C)]
pub struct VkLayerDeviceLink {
    pub p_next: *mut VkLayerDeviceLink,
    pub pfn_next_get_instance_proc_addr: PFN_vkGetInstanceProcAddr,
    pub pfn_next_get_device_proc_addr: PFN_vkGetDeviceProcAddr,
}

#[repr(C)]
pub union VkLayerDeviceCreateInfoUnion {
    pub p_layer_info: *mut VkLayerDeviceLink,
    pub pfn_set_device_loader_data: *mut c_void,
}

#[repr(C)]
pub struct VkLayerDeviceCreateInfo {
    pub s_type: i32,
    pub p_next: *const VkBaseInStructure,
    pub function: i32,
    pub u: VkLayerDeviceCreateInfoUnion,
}

/// What the loader hands us from `vkNegotiateLoaderLayerInterfaceVersion`.
#[repr(C)]
pub struct VkNegotiateLayerInterface {
    pub s_type: i32,
    pub p_next: *mut c_void,
    pub loader_layer_interface_version: u32,
    pub pfn_get_instance_proc_addr: Option<PFN_vkGetInstanceProcAddr>,
    pub pfn_get_device_proc_addr: Option<PFN_vkGetDeviceProcAddr>,
    pub pfn_get_physical_device_proc_addr: *mut c_void,
}

/// Finds the loader's link struct in a create-info `pNext` chain.
///
/// # Safety
///
/// `head` must be a valid `pNext` chain: either null or a pointer to a struct
/// whose first two fields are `sType` and `pNext`, recursively.
pub unsafe fn find_chain_info(
    head: *const VkBaseInStructure,
    s_type: i32,
) -> *const VkBaseInStructure {
    let mut node = head;
    while !node.is_null() {
        // SAFETY: the caller guarantees `node` points at a valid chain entry.
        let entry = unsafe { &*node };
        if entry.s_type == s_type {
            // Both loader create-infos put `function` immediately after
            // `pNext`, so this read is valid for either.
            let function = unsafe { (*node.cast::<VkLayerInstanceCreateInfo>()).function };
            if function == VK_LAYER_LINK_INFO {
                return node;
            }
        }
        node = entry.p_next;
    }
    std::ptr::null()
}
