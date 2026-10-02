//! A minimal Vulkan client, because `vulkaninfo` cannot be used headless.
//!
//! `vulkaninfo` builds an Xlib window unconditionally and segfaults tearing it
//! down on a machine with no X server — before flushing any output, so it can
//! neither report a device nor fail cleanly. This does the smallest thing that
//! exercises every entry point the present layer hooks: create an instance,
//! enumerate physical devices, create a logical device, and fetch a queue.
//!
//! The loader is opened with `dlopen` rather than linked, so the probe needs no
//! Vulkan at build time and stays useful on a machine with no driver at all.

use std::ffi::{c_char, c_void, CStr};
use std::mem;
use std::ptr;

use anyhow::{bail, Result};

type VkInstance = *mut c_void;
type VkPhysicalDevice = *mut c_void;
type VkDevice = *mut c_void;
type VkQueue = *mut c_void;
type VkResult = i32;

const VK_SUCCESS: VkResult = 0;
const VK_STRUCTURE_TYPE_APPLICATION_INFO: i32 = 0;
const VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO: i32 = 1;
const VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO: i32 = 2;
const VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO: i32 = 3;
/// Vulkan 1.0.0, so this runs against any conformant driver.
const VK_API_VERSION_1_0: u32 = 1 << 22;

type PfnVoid = Option<unsafe extern "system" fn()>;
type PfnGetInstanceProcAddr = unsafe extern "system" fn(VkInstance, *const c_char) -> PfnVoid;
type PfnGetDeviceProcAddr = unsafe extern "system" fn(VkDevice, *const c_char) -> PfnVoid;
type PfnCreateInstance = unsafe extern "system" fn(
    *const VkInstanceCreateInfo,
    *const c_void,
    *mut VkInstance,
) -> VkResult;
type PfnDestroyInstance = unsafe extern "system" fn(VkInstance, *const c_void);
type PfnEnumeratePhysicalDevices =
    unsafe extern "system" fn(VkInstance, *mut u32, *mut VkPhysicalDevice) -> VkResult;
type PfnGetQueueFamilyProperties =
    unsafe extern "system" fn(VkPhysicalDevice, *mut u32, *mut VkQueueFamilyProperties);
type PfnCreateDevice = unsafe extern "system" fn(
    VkPhysicalDevice,
    *const VkDeviceCreateInfo,
    *const c_void,
    *mut VkDevice,
) -> VkResult;
type PfnDestroyDevice = unsafe extern "system" fn(VkDevice, *const c_void);
type PfnGetDeviceQueue = unsafe extern "system" fn(VkDevice, u32, u32, *mut VkQueue);

#[repr(C)]
struct VkApplicationInfo {
    s_type: i32,
    p_next: *const c_void,
    p_application_name: *const c_char,
    application_version: u32,
    p_engine_name: *const c_char,
    engine_version: u32,
    api_version: u32,
}

#[repr(C)]
struct VkInstanceCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    p_application_info: *const VkApplicationInfo,
    enabled_layer_count: u32,
    pp_enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    pp_enabled_extension_names: *const *const c_char,
}

#[repr(C)]
struct VkExtent3D {
    width: u32,
    height: u32,
    depth: u32,
}

#[repr(C)]
struct VkQueueFamilyProperties {
    queue_flags: u32,
    queue_count: u32,
    timestamp_valid_bits: u32,
    min_image_transfer_granularity: VkExtent3D,
}

#[repr(C)]
struct VkDeviceQueueCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    queue_family_index: u32,
    queue_count: u32,
    p_queue_priorities: *const f32,
}

#[repr(C)]
struct VkDeviceCreateInfo {
    s_type: i32,
    p_next: *const c_void,
    flags: u32,
    queue_create_info_count: u32,
    p_queue_create_infos: *const VkDeviceQueueCreateInfo,
    enabled_layer_count: u32,
    pp_enabled_layer_names: *const *const c_char,
    enabled_extension_count: u32,
    pp_enabled_extension_names: *const *const c_char,
    p_enabled_features: *const c_void,
}

/// What the client managed to do. Each field is a step the present layer sits
/// in the middle of, so a layer that breaks the chain changes one of them.
pub struct Report {
    pub physical_devices: u32,
    pub queue_families: u32,
    pub created_device: bool,
    pub got_queue: bool,
    pub present_resolved: bool,
}

impl Report {
    #[must_use]
    pub fn to_json(&self) -> String {
        format!(
            r#"{{"physical_devices":{},"queue_families":{},"created_device":{},"got_queue":{},"present_resolved":{}}}"#,
            self.physical_devices,
            self.queue_families,
            self.created_device,
            self.got_queue,
            self.present_resolved
        )
    }
}

/// Resolves one entry point, by name, through a `vkGet*ProcAddr`.
macro_rules! resolve {
    ($getter:expr, $handle:expr, $name:literal, $ty:ty) => {{
        // SAFETY: the getter came from the loader and the handle is one it
        // accepts; the loader guarantees the returned pointer matches `$name`.
        let f = unsafe { $getter($handle, $name.as_ptr()) };
        match f {
            // The source type is spelled out because every Vulkan entry point
            // arrives as the same opaque `PFN_vkVoidFunction`, and the name it
            // was asked for is what fixes its real signature.
            Some(f) => Ok::<$ty, anyhow::Error>(unsafe {
                mem::transmute::<unsafe extern "system" fn(), $ty>(f)
            }),
            None => bail!(concat!("the loader could not resolve ", stringify!($name))),
        }
    }};
}

/// Creates an instance, a device and a queue, and reports what happened.
pub fn probe() -> Result<Report> {
    let gipa = load_loader()?;

    let create_instance: PfnCreateInstance = resolve!(
        gipa,
        ptr::null_mut(),
        c"vkCreateInstance",
        PfnCreateInstance
    )?;

    let app = VkApplicationInfo {
        s_type: VK_STRUCTURE_TYPE_APPLICATION_INFO,
        p_next: ptr::null(),
        p_application_name: c"lagd-probe".as_ptr(),
        application_version: 0,
        p_engine_name: ptr::null(),
        engine_version: 0,
        api_version: VK_API_VERSION_1_0,
    };
    let instance_info = VkInstanceCreateInfo {
        s_type: VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        p_next: ptr::null(),
        flags: 0,
        p_application_info: &raw const app,
        enabled_layer_count: 0,
        pp_enabled_layer_names: ptr::null(),
        enabled_extension_count: 0,
        pp_enabled_extension_names: ptr::null(),
    };

    let mut instance: VkInstance = ptr::null_mut();
    // SAFETY: both structs are fully initialised and outlive the call.
    let rc = unsafe { create_instance(&raw const instance_info, ptr::null(), &raw mut instance) };
    if rc != VK_SUCCESS {
        bail!("vkCreateInstance failed with VkResult {rc}");
    }

    let result = probe_instance(gipa, instance);

    // Tear down even on failure: the layer's vkDestroyInstance hook is part of
    // what is under test.
    if let Ok(destroy) = resolve!(gipa, instance, c"vkDestroyInstance", PfnDestroyInstance) {
        // SAFETY: `instance` was created above and is not used afterwards.
        unsafe { destroy(instance, ptr::null()) };
    }
    result
}

// `gipa` and `gdpa` differ by one letter deliberately: they are the loader
// interface's own abbreviations for the two resolvers.
#[allow(clippy::similar_names)]
fn probe_instance(gipa: PfnGetInstanceProcAddr, instance: VkInstance) -> Result<Report> {
    let enumerate: PfnEnumeratePhysicalDevices = resolve!(
        gipa,
        instance,
        c"vkEnumeratePhysicalDevices",
        PfnEnumeratePhysicalDevices
    )?;

    let mut count: u32 = 0;
    // SAFETY: the null data pointer is the documented way to ask for the count.
    let rc = unsafe { enumerate(instance, &raw mut count, ptr::null_mut()) };
    if rc != VK_SUCCESS {
        bail!("vkEnumeratePhysicalDevices failed with VkResult {rc}");
    }
    if count == 0 {
        return Ok(Report {
            physical_devices: 0,
            queue_families: 0,
            created_device: false,
            got_queue: false,
            present_resolved: false,
        });
    }

    let mut devices: Vec<VkPhysicalDevice> = vec![ptr::null_mut(); count as usize];
    // SAFETY: `devices` has room for `count` handles.
    let rc = unsafe { enumerate(instance, &raw mut count, devices.as_mut_ptr()) };
    if rc != VK_SUCCESS {
        bail!("vkEnumeratePhysicalDevices (fill) failed with VkResult {rc}");
    }
    let physical = devices[0];

    // Queue family 0 is not guaranteed to exist on paper, so ask.
    let families: PfnGetQueueFamilyProperties = resolve!(
        gipa,
        instance,
        c"vkGetPhysicalDeviceQueueFamilyProperties",
        PfnGetQueueFamilyProperties
    )?;
    let mut family_count: u32 = 0;
    // SAFETY: null data pointer asks for the count.
    unsafe { families(physical, &raw mut family_count, ptr::null_mut()) };
    if family_count == 0 {
        bail!("the physical device reports no queue families");
    }

    let create_device: PfnCreateDevice =
        resolve!(gipa, instance, c"vkCreateDevice", PfnCreateDevice)?;

    let priority = 1.0f32;
    let queue_info = VkDeviceQueueCreateInfo {
        s_type: VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        p_next: ptr::null(),
        flags: 0,
        queue_family_index: 0,
        queue_count: 1,
        p_queue_priorities: &raw const priority,
    };
    let device_info = VkDeviceCreateInfo {
        s_type: VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        p_next: ptr::null(),
        flags: 0,
        queue_create_info_count: 1,
        p_queue_create_infos: &raw const queue_info,
        enabled_layer_count: 0,
        pp_enabled_layer_names: ptr::null(),
        enabled_extension_count: 0,
        pp_enabled_extension_names: ptr::null(),
        p_enabled_features: ptr::null(),
    };

    let mut device: VkDevice = ptr::null_mut();
    // SAFETY: both structs are fully initialised and outlive the call.
    let rc = unsafe {
        create_device(
            physical,
            &raw const device_info,
            ptr::null(),
            &raw mut device,
        )
    };
    if rc != VK_SUCCESS {
        bail!("vkCreateDevice failed with VkResult {rc}");
    }

    let gdpa: PfnGetDeviceProcAddr =
        resolve!(gipa, instance, c"vkGetDeviceProcAddr", PfnGetDeviceProcAddr)?;

    // This is the hook that lets the layer map a queue back to its device, so
    // it has to work or `vkQueuePresentKHR` could never find its way down.
    let get_queue: PfnGetDeviceQueue =
        resolve!(gdpa, device, c"vkGetDeviceQueue", PfnGetDeviceQueue)?;
    let mut queue: VkQueue = ptr::null_mut();
    // SAFETY: family 0 and index 0 were requested at device creation.
    unsafe { get_queue(device, 0, 0, &raw mut queue) };

    // Without VK_KHR_swapchain enabled this is expected to be absent; the layer
    // must report it the same way the driver does rather than claim it.
    // SAFETY: `device` is live and the name is a valid C string.
    let present_resolved = unsafe { gdpa(device, c"vkQueuePresentKHR".as_ptr()) }.is_some();

    if let Ok(destroy) = resolve!(gdpa, device, c"vkDestroyDevice", PfnDestroyDevice) {
        // SAFETY: `device` is not used after this.
        unsafe { destroy(device, ptr::null()) };
    }

    Ok(Report {
        physical_devices: count,
        queue_families: family_count,
        created_device: true,
        got_queue: !queue.is_null(),
        present_resolved,
    })
}

/// `dlopen`s the Vulkan loader and pulls out its `vkGetInstanceProcAddr`.
fn load_loader() -> Result<PfnGetInstanceProcAddr> {
    // SAFETY: a valid C string naming the loader's soname.
    let handle = unsafe { libc::dlopen(c"libvulkan.so.1".as_ptr(), libc::RTLD_NOW) };
    if handle.is_null() {
        bail!("dlopen libvulkan.so.1: {}", dlerror());
    }
    // Deliberately never closed: the entry points stay live for the process.
    // SAFETY: `handle` is a live dlopen handle and the name is a C string.
    let sym = unsafe { libc::dlsym(handle, c"vkGetInstanceProcAddr".as_ptr()) };
    if sym.is_null() {
        bail!("the loader exports no vkGetInstanceProcAddr: {}", dlerror());
    }
    // SAFETY: the symbol is the loader's own entry point, whose signature is
    // fixed by the Vulkan ABI.
    Ok(unsafe { mem::transmute::<*mut c_void, PfnGetInstanceProcAddr>(sym) })
}

fn dlerror() -> String {
    // SAFETY: dlerror returns either null or a valid C string.
    let msg = unsafe { libc::dlerror() };
    if msg.is_null() {
        return "no error reported".to_owned();
    }
    // SAFETY: non-null dlerror results are NUL-terminated.
    unsafe { CStr::from_ptr(msg) }
        .to_string_lossy()
        .into_owned()
}
