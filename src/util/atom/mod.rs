/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

mod data;

// Careful which things we derive, because we need to maintain equivalent
// behavior between an interned and a non-interned string.
/// Interned string.
#[deriving(Clone)]
pub enum Atom {
    Static(&'static str),
    // dynamic interning goes here
    Owned(~str),
}

impl Atom {
    pub fn as_slice<'t>(&'t self) -> &'t str {
        match *self {
            Static(r) => r,
            Owned(ref s) => s.as_slice(),
        }
    }
}

impl Eq for Atom {
    fn eq(&self, other: &Atom) -> bool {
        match (self, other) {
            (&Static(x), &Static(y)) => x.as_ptr() == y.as_ptr(),
            (x, y) => x.as_slice() == y.as_slice(),
        }
    }
}

impl TotalEq for Atom { }
