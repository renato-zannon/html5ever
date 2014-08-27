// Copyright 2014 The html5ever Project Developers. See the
// COPYRIGHT file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ptr::RawPtr;
use std::slice::raw::buf_as_slice;
use std::str::raw::from_utf8;

use libc::{size_t, c_int};

#[repr(C)]
pub struct h5e_buf {
    data: *const u8,
    len: size_t,
}

impl h5e_buf {
    pub fn from_slice(x: &str) -> h5e_buf {
        h5e_buf {
            data: x.as_bytes().as_ptr(),
            len: x.len() as size_t,
        }
    }

    pub fn null() -> h5e_buf {
        h5e_buf {
            data: RawPtr::null(),
            len: 0,
        }
    }

    pub unsafe fn with_slice<R>(&self, f: |&str| -> R) -> R {
        buf_as_slice(self.data, self.len as uint,
            |bytes| f(from_utf8(bytes)))
    }
}

pub fn c_bool(x: bool) -> c_int {
    match x {
        false => 0,
        true => 1,
    }
}
