use crate::builtins::{PyStrRef, PyTypeRef};
use crate::common::lock::PyRwLock;
use crate::function::OptionalArg;
use crate::slots::{PyTpGetattro, PyTpNew};
use crate::vm::VirtualMachine;
use crate::{PyObjectRef, PyResult, PyValue, pyclass};
use libffi::middle::Abi;
use once_cell::sync::Lazy;
use std::collections::HashMap;
// Assuming PyCFuncPtr will be accessible from super::function module
use super::function::PyCFuncPtr; 
use libloading::Library;

// TODO: Move LIBCACHE to a more appropriate location, possibly within the vm or a dedicated module.
// For now, it's here to allow PyCDLL to access it.
type LibCache = PyRwLock<HashMap<String, Library>>;
static LIBCACHE: Lazy<LibCache> = Lazy::new(Default::default);

#[pyclass(name = "CDLL", module = "_ctypes")]
#[derive(Debug)]
pub struct PyCDLL {
    // Store the name for now, as PyObjectRef cannot directly hold a Library.
    // We'll use this name to retrieve the Library from LIBCACHE when needed.
    library_name: String, 
    default_abi: Abi,
}

#[pyclass]
impl PyCDLL {
    #[pyslot]
    fn py_new(
        cls: PyTypeRef,
        name: PyStrRef,
        mode: OptionalArg<i32>, // mode is for dlopen flags, unused for now
        handle: OptionalArg<PyObjectRef>, // handle allows using an already opened library, unused for now
        use_errno: OptionalArg<bool>, // For POSIX, copies errno, unused for now
        use_last_error: OptionalArg<bool>, // For Windows, copies GetLastError, unused for now
        vm: &VirtualMachine,
    ) -> PyResult<Self> {
        let library_path = name.as_str();
        
        // Simplified library loading:
        // - Ignores mode, handle, use_errno, use_last_error for now.
        // - Error handling needs to be more robust.
        let mut lib_cache_guard = LIBCACHE.write();
        if !lib_cache_guard.contains_key(library_path) {
            match unsafe { Library::new(library_path) } {
                Ok(lib) => {
                    lib_cache_guard.insert(library_path.to_string(), lib);
                }
                Err(e) => return Err(vm.new_os_error(format!("Failed to load library '{}': {}", library_path, e))),
            }
        }
        // Drop the write guard soon as possible
        drop(lib_cache_guard);

        Ok(PyCDLL {
            library_name: library_path.to_string(),
            default_abi: Abi::Cdecl,
        })
    }

    #[pymethod]
    fn __getattr__(&self, name: PyStrRef, vm: &VirtualMachine) -> PyResult {
        // Check if the library is still loaded
        let lib_cache_guard = LIBCACHE.read();
        let _library = lib_cache_guard.get(&self.library_name)
            .ok_or_else(|| vm.new_os_error(format!("Library {} not found in cache or unloaded", self.library_name)))?;
        // Drop the read guard
        drop(lib_cache_guard);

        // Create and return a PyCFuncPtr instance.
        // This requires PyCFuncPtr::new (or a similar constructor) to accept the library name (or handle)
        // and the ABI.
        // For now, we'll assume PyCFuncPtr::new can take these.
        // The actual Symbol<T> loading will happen within PyCFuncPtr when it's called.
        PyCFuncPtr::new_for_dll(
            name.to_owned(), // function name
            self.library_name.clone(), // library name/identifier
            self.default_abi, // calling convention
            vm,
        )
    }
}

impl PyTpGetattro for PyCDLL {
    fn getattro(zelf: &crate::Py<Self>, name_str: PyStrRef, vm: &VirtualMachine) -> PyResult {
        // Delegate to __getattr__ pymethod
        Self::__getattr__(zelf, name_str, vm)
    }
}

pub(super) fn init_type(vm: &VirtualMachine, module: &PyObjectRef, typ: &PyTypeRef) {
    PyCDLL::extend_class(&vm.ctx, typ);
    PyWinDLL::extend_class(&vm.ctx, typ);
    PyOleDLL::extend_class(&vm.ctx, typ);
    PyPyDLL::extend_class(&vm.ctx, typ);
    // Any other type specific initializations for PyCDLL
}

// This function is not strictly necessary if init_type is used by make_module,
// but can be kept if direct access to PyCDLL type is needed elsewhere.
pub fn make_ ctypes_cdll_type(ctx: &crate::Context) -> PyTypeRef {
    PyCDLL::class_with_opts(ctx, crate::builtins::PyType::static_type())
}

// PyWinDLL Implementation
#[pyclass(name = "WinDLL", module = "_ctypes")]
#[derive(Debug)]
pub struct PyWinDLL {
    library_name: String,
    default_abi: Abi,
}

#[pyclass]
impl PyWinDLL {
    #[pyslot]
    fn py_new(
        cls: PyTypeRef,
        name: PyStrRef,
        mode: OptionalArg<i32>,
        handle: OptionalArg<PyObjectRef>,
        use_errno: OptionalArg<bool>,
        use_last_error: OptionalArg<bool>,
        vm: &VirtualMachine,
    ) -> PyResult<Self> {
        let library_path = name.as_str();
        let mut lib_cache_guard = LIBCACHE.write();
        if !lib_cache_guard.contains_key(library_path) {
            match unsafe { Library::new(library_path) } {
                Ok(lib) => {
                    lib_cache_guard.insert(library_path.to_string(), lib);
                }
                Err(e) => return Err(vm.new_os_error(format!("Failed to load library '{}': {}", library_path, e))),
            }
        }
        drop(lib_cache_guard);

        Ok(PyWinDLL {
            library_name: library_path.to_string(),
            default_abi: Abi::Stdcall, // Key difference for WinDLL
        })
    }

    #[pymethod]
    fn __getattr__(&self, name: PyStrRef, vm: &VirtualMachine) -> PyResult {
        let lib_cache_guard = LIBCACHE.read();
        let _library = lib_cache_guard.get(&self.library_name)
            .ok_or_else(|| vm.new_os_error(format!("Library {} not found in cache or unloaded", self.library_name)))?;
        drop(lib_cache_guard);

        PyCFuncPtr::new_for_dll(
            name.to_owned(),
            self.library_name.clone(),
            self.default_abi, // Use Stdcall
            vm,
        )
    }
}

impl PyTpGetattro for PyWinDLL {
    fn getattro(zelf: &crate::Py<Self>, name_str: PyStrRef, vm: &VirtualMachine) -> PyResult {
        Self::__getattr__(zelf, name_str, vm)
    }
}

pub fn make_ ctypes_windll_type(ctx: &crate::Context) -> PyTypeRef {
    PyWinDLL::class_with_opts(ctx, crate::builtins::PyType::static_type())
}


// PyOleDLL Implementation
#[pyclass(name = "OleDLL", module = "_ctypes")]
#[derive(Debug)]
pub struct PyOleDLL {
    library_name: String,
    default_abi: Abi,
}

#[pyclass]
impl PyOleDLL {
    #[pyslot]
    fn py_new(
        cls: PyTypeRef,
        name: PyStrRef,
        mode: OptionalArg<i32>,
        handle: OptionalArg<PyObjectRef>,
        use_errno: OptionalArg<bool>,
        use_last_error: OptionalArg<bool>,
        vm: &VirtualMachine,
    ) -> PyResult<Self> {
        let library_path = name.as_str();
        let mut lib_cache_guard = LIBCACHE.write();
        if !lib_cache_guard.contains_key(library_path) {
            match unsafe { Library::new(library_path) } {
                Ok(lib) => {
                    lib_cache_guard.insert(library_path.to_string(), lib);
                }
                Err(e) => return Err(vm.new_os_error(format!("Failed to load library '{}': {}", library_path, e))),
            }
        }
        drop(lib_cache_guard);

        Ok(PyOleDLL {
            library_name: library_path.to_string(),
            default_abi: Abi::Stdcall, // Key difference for OleDLL
        })
    }

    #[pymethod]
    fn __getattr__(&self, name: PyStrRef, vm: &VirtualMachine) -> PyResult {
        let lib_cache_guard = LIBCACHE.read();
        let _library = lib_cache_guard.get(&self.library_name)
            .ok_or_else(|| vm.new_os_error(format!("Library {} not found in cache or unloaded", self.library_name)))?;
        drop(lib_cache_guard);

        PyCFuncPtr::new_for_dll(
            name.to_owned(),
            self.library_name.clone(),
            self.default_abi, // Use Stdcall
            vm,
        )
    }
}

impl PyTpGetattro for PyOleDLL {
    fn getattro(zelf: &crate::Py<Self>, name_str: PyStrRef, vm: &VirtualMachine) -> PyResult {
        Self::__getattr__(zelf, name_str, vm)
    }
}

pub fn make_ ctypes_oledll_type(ctx: &crate::Context) -> PyTypeRef {
    PyOleDLL::class_with_opts(ctx, crate::builtins::PyType::static_type())
}


// PyPyDLL Implementation
#[pyclass(name = "PyDLL", module = "_ctypes")]
#[derive(Debug)]
pub struct PyPyDLL {
    library_name: String,
    default_abi: Abi,
}

#[pyclass]
impl PyPyDLL {
    #[pyslot]
    fn py_new(
        cls: PyTypeRef,
        name: PyStrRef,
        mode: OptionalArg<i32>,
        handle: OptionalArg<PyObjectRef>,
        // PyDLL doesn't use use_errno or use_last_error in CPython _ctypes.c
        // but keeping them for signature consistency for now.
        _use_errno: OptionalArg<bool>,
        _use_last_error: OptionalArg<bool>,
        vm: &VirtualMachine,
    ) -> PyResult<Self> {
        let library_path = name.as_str();
        let mut lib_cache_guard = LIBCACHE.write();
        if !lib_cache_guard.contains_key(library_path) {
            match unsafe { Library::new(library_path) } {
                Ok(lib) => {
                    lib_cache_guard.insert(library_path.to_string(), lib);
                }
                Err(e) => return Err(vm.new_os_error(format!("Failed to load library '{}': {}", library_path, e))),
            }
        }
        drop(lib_cache_guard);

        Ok(PyPyDLL {
            library_name: library_path.to_string(),
            default_abi: Abi::Cdecl, // Key difference for PyDLL
        })
    }

    #[pymethod]
    fn __getattr__(&self, name: PyStrRef, vm: &VirtualMachine) -> PyResult {
        let lib_cache_guard = LIBCACHE.read();
        let _library = lib_cache_guard.get(&self.library_name)
            .ok_or_else(|| vm.new_os_error(format!("Library {} not found in cache or unloaded", self.library_name)))?;
        drop(lib_cache_guard);

        PyCFuncPtr::new_for_dll(
            name.to_owned(),
            self.library_name.clone(),
            self.default_abi, // Use Cdecl
            vm,
        )
    }
}

impl PyTpGetattro for PyPyDLL {
    fn getattro(zelf: &crate::Py<Self>, name_str: PyStrRef, vm: &VirtualMachine) -> PyResult {
        Self::__getattr__(zelf, name_str, vm)
    }
}

pub fn make_ ctypes_pydll_type(ctx: &crate::Context) -> PyTypeRef {
    PyPyDLL::class_with_opts(ctx, crate::builtins::PyType::static_type())
}
