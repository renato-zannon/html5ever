// Copyright 2014 The html5ever Project Developers. See the
// COPYRIGHT file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#![warn(warnings)]

use common::{h5e_buf, c_bool};

use html5ever::tokenizer::{TokenSink, Token, Doctype, Tag, ParseError, DoctypeToken};
use html5ever::tokenizer::{CommentToken, CharacterTokens, NullCharacterToken};
use html5ever::tokenizer::{TagToken, StartTag, EndTag, EOFToken, Tokenizer};

use std::mem;
use std::default::Default;
use libc::{c_void, c_int, size_t};

#[repr(C)]
pub struct h5e_token_ops {
    do_doctype:            extern "C" fn(user: *mut c_void, name: h5e_buf, public: h5e_buf, system: h5e_buf, force_quirks: c_int),
    do_start_tag:          extern "C" fn(user: *mut c_void, name: h5e_buf, self_closing: c_int, num_attrs: size_t),
    do_tag_attr:           extern "C" fn(user: *mut c_void, name: h5e_buf, value: h5e_buf),
    do_end_tag:            extern "C" fn(user: *mut c_void, name: h5e_buf),
    do_comment:            extern "C" fn(user: *mut c_void, text: h5e_buf),
    do_characters:         extern "C" fn(user: *mut c_void, text: h5e_buf),
    do_null_character:     extern "C" fn(user: *mut c_void),
    do_eof:                extern "C" fn(user: *mut c_void),
    do_error:              extern "C" fn(user: *mut c_void, message: h5e_buf),
}

#[repr(C)]
pub struct h5e_token_sink {
    ops: *const h5e_token_ops,
    user: *mut c_void,
}

impl TokenSink for h5e_token_sink {
    fn process_token(&mut self, token: Token) {
        macro_rules! call ( ($name:ident $(, $arg:expr)*) => (
            unsafe {
                if !((*self.ops).$name as *const ()).is_null() {
                    ((*(self.ops)).$name)(self.user $(, $arg)*);
                }
            }
        ))

        fn str_to_buf(s: &String) -> h5e_buf {
            h5e_buf::from_slice(s.as_slice())
        }

        fn opt_str_to_buf(s: &Option<String>) -> h5e_buf {
            match *s {
                None => h5e_buf::null(),
                Some(ref s) => h5e_buf::from_slice(s.as_slice()),
            }
        }

        match token {
            DoctypeToken(Doctype { ref name, ref public_id, ref system_id, force_quirks })
                => call!(do_doctype,
                    opt_str_to_buf(name),
                    opt_str_to_buf(public_id),
                    opt_str_to_buf(system_id),
                    c_bool(force_quirks)),

            TagToken(Tag { kind, name, self_closing, attrs }) => {
                match kind {
                    StartTag => {
                        call!(do_start_tag, h5e_buf::from_slice(name.as_slice()),
                            c_bool(self_closing), attrs.len() as size_t);
                        for attr in attrs.move_iter() {
                            call!(do_tag_attr, h5e_buf::from_slice(name.as_slice()),
                                str_to_buf(&attr.value))
                        }
                    }
                    EndTag => call!(do_end_tag, h5e_buf::from_slice(name.as_slice())),
                }
            }

            CommentToken(text) => call!(do_comment, str_to_buf(&text)),

            CharacterTokens(text) => call!(do_comment, str_to_buf(&text)),

            NullCharacterToken => call!(do_null_character),

            EOFToken => call!(do_eof),

            ParseError(msg) => call!(do_error, h5e_buf::from_slice(msg.as_slice())),
        }
    }
}

pub type h5e_tokenizer_ptr = *const ();

#[no_mangle]
pub unsafe extern "C" fn h5e_tokenizer_new(sink: *mut h5e_token_sink) -> h5e_tokenizer_ptr {
    let tok: Box<Tokenizer<h5e_token_sink>>
        = box Tokenizer::new(mem::transmute::<_, &mut h5e_token_sink>(sink),
            Default::default());

    mem::transmute(tok)
}

#[no_mangle]
pub unsafe extern "C" fn h5e_tokenizer_free(tok: h5e_tokenizer_ptr) {
    let _: Box<Tokenizer<h5e_token_sink>> = mem::transmute(tok);
}

#[no_mangle]
pub unsafe extern "C" fn h5e_tokenizer_feed(tok: h5e_tokenizer_ptr, buf: h5e_buf) {
    let tok: &mut Tokenizer<h5e_token_sink> = mem::transmute(tok);
    tok.feed(buf.with_slice(|s| s.to_string()));
}

#[no_mangle]
pub unsafe extern "C" fn h5e_tokenizer_end(tok: h5e_tokenizer_ptr) {
    let tok: &mut Tokenizer<h5e_token_sink> = mem::transmute(tok);
    tok.end();
}
