// Copyright 2014 The html5ever Project Developers. See the
// COPYRIGHT file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! The HTML5 tokenizer.

pub use self::interface::{Doctype, Attribute, AttrName, TagKind, StartTag, EndTag, Tag};
pub use self::interface::{Token, DoctypeToken, TagToken, CommentToken};
pub use self::interface::{CharacterTokens, NullCharacterToken, EOFToken, ParseError};
pub use self::interface::TokenSink;

use self::states::{RawLessThanSign, RawEndTagOpen, RawEndTagName};
use self::states::{Rcdata, Rawtext, ScriptData, ScriptDataEscaped};
use self::states::{Escaped, DoubleEscaped};
use self::states::{Unquoted, SingleQuoted, DoubleQuoted};
use self::states::{DoctypeIdKind, Public, System};

use self::char_ref::{CharRef, CharRefTokenizer};

use self::buffer_queue::{BufferQueue, SetResult, FromSet, NotFromSet};

use util::str::{lower_ascii, lower_ascii_letter, empty_str};
use util::smallcharset::SmallCharSet;

use std::ascii::StrAsciiExt;
use std::mem::replace;
use std::iter::AdditiveIterator;
use std::default::Default;
use std::str::{MaybeOwned, Slice, Owned};

use std::collections::hashmap::HashMap;

use string_cache::Atom;

pub mod states;
mod interface;
mod char_ref;
mod buffer_queue;

fn option_push_char(opt_str: &mut Option<String>, c: char) {
    match *opt_str {
        Some(ref mut s) => s.push_char(c),
        None => *opt_str = Some(String::from_char(1, c)),
    }
}

fn append_strings(lhs: &mut String, rhs: String) {
    if lhs.is_empty() {
        *lhs = rhs;
    } else {
        lhs.push_str(rhs.as_slice());
    }
}

/// Tokenizer options, with an impl for `Default`.
#[deriving(Clone)]
pub struct TokenizerOpts {
    /// Report all parse errors described in the spec, at some
    /// performance penalty?  Default: false
    pub exact_errors: bool,

    /// Discard a `U+FEFF BYTE ORDER MARK` if we see one at the beginning
    /// of the stream?  Default: true
    pub discard_bom: bool,

    /// Keep a record of how long we spent in each state?  Printed
    /// when `end()` is called.  Default: false
    pub profile: bool,

    /// Initial state override.  Only the test runner should use
    /// a non-`None` value!
    pub initial_state: Option<states::State>,

    /// Last start tag.  Only the test runner should use a
    /// non-`None` value!
    pub last_start_tag_name: Option<String>,
}

impl Default for TokenizerOpts {
    fn default() -> TokenizerOpts {
        TokenizerOpts {
            exact_errors: false,
            discard_bom: true,
            profile: false,
            initial_state: None,
            last_start_tag_name: None,
        }
    }
}

/// The HTML tokenizer.
pub struct Tokenizer<'sink, Sink> {
    /// Options controlling the behavior of the tokenizer.
    opts: TokenizerOpts,

    /// Destination for tokens we emit.
    sink: &'sink mut Sink,

    /// The abstract machine state as described in the spec.
    state: states::State,

    /// Input ready to be tokenized.
    input_buffers: BufferQueue,

    /// If Some(n), the abstract machine needs n available
    /// characters to continue.
    wait_for: Option<uint>,

    /// Are we at the end of the file, once buffers have been processed
    /// completely? This affects whether we will wait for lookahead or not.
    at_eof: bool,

    /// Tokenizer for character references, if we're tokenizing
    /// one at the moment.
    char_ref_tokenizer: Option<Box<CharRefTokenizer>>,

    /// Current input character.  Just consumed, may reconsume.
    current_char: char,

    /// Should we reconsume the current input character?
    reconsume: bool,

    /// Did we just consume \r, translating it to \n?  In that case we need
    /// to ignore the next character if it's \n.
    ignore_lf: bool,

    /// Discard a U+FEFF BYTE ORDER MARK if we see one?  Only done at the
    /// beginning of the stream.
    discard_bom: bool,

    /// Current tag kind.
    current_tag_kind: TagKind,

    /// Current tag name.
    current_tag_name: String,

    /// Current tag is self-closing?
    current_tag_self_closing: bool,

    /// Current tag attributes.
    current_tag_attrs: Vec<Attribute>,

    /// Current attribute name.
    current_attr_name: String,

    /// Current attribute value.
    current_attr_value: String,

    /// Current comment.
    current_comment: String,

    /// Current doctype token.
    current_doctype: Doctype,

    /// Last start tag name, for use in checking "appropriate end tag".
    last_start_tag_name: Option<Atom>,

    /// The "temporary buffer" mentioned in the spec.
    temp_buf: String,

    /// Record of how many ns we spent in each state, if profiling is enabled.
    state_profile: HashMap<states::State, u64>,

    /// Record of how many ns we spent in the token sink.
    time_in_sink: u64,
}

impl<'sink, Sink: TokenSink> Tokenizer<'sink, Sink> {
    /// Create a new tokenizer which feeds tokens to a particular `TokenSink`.
    pub fn new(sink: &'sink mut Sink, mut opts: TokenizerOpts) -> Tokenizer<'sink, Sink> {
        let start_tag_name = opts.last_start_tag_name.take()
            .map(|s| Atom::from_slice(s.as_slice()));
        let state = *opts.initial_state.as_ref().unwrap_or(&states::Data);
        let discard_bom = opts.discard_bom;
        Tokenizer {
            opts: opts,
            sink: sink,
            state: state,
            wait_for: None,
            char_ref_tokenizer: None,
            input_buffers: BufferQueue::new(),
            at_eof: false,
            current_char: '\0',
            reconsume: false,
            ignore_lf: false,
            discard_bom: discard_bom,
            current_tag_kind: StartTag,
            current_tag_name: empty_str(),
            current_tag_self_closing: false,
            current_tag_attrs: vec!(),
            current_attr_name: empty_str(),
            current_attr_value: empty_str(),
            current_comment: empty_str(),
            current_doctype: Doctype::new(),
            last_start_tag_name: start_tag_name,
            temp_buf: empty_str(),
            state_profile: HashMap::new(),
            time_in_sink: 0,
        }
    }

    /// Feed an input string into the tokenizer.
    pub fn feed(&mut self, input: String) {
        if input.len() == 0 {
            return;
        }

        let pos = if self.discard_bom && input.as_slice().char_at(0) == '\ufeff' {
            self.discard_bom = false;
            3  // length of BOM in UTF-8
        } else {
            0
        };

        self.input_buffers.push_back(input, pos);
        self.run();
    }

    fn process_token(&mut self, token: Token) {
        if self.opts.profile {
            let (_, dt) = time!(self.sink.process_token(token));
            self.time_in_sink += dt;
        } else {
            self.sink.process_token(token);
        }
    }

    //§ preprocessing-the-input-stream
    // Get the next input character, which might be the character
    // 'c' that we already consumed from the buffers.
    fn get_preprocessed_char(&mut self, mut c: char) -> Option<char> {
        if self.ignore_lf {
            self.ignore_lf = false;
            if c == '\n' {
                c = unwrap_or_return!(self.input_buffers.next(), None);
            }
        }

        if c == '\r' {
            self.ignore_lf = true;
            c = '\n';
        }

        if self.opts.exact_errors && match c as u32 {
            0x01..0x08 | 0x0B | 0x0E..0x1F | 0x7F..0x9F | 0xFDD0..0xFDEF => true,
            n if (n & 0xFFFE) == 0xFFFE => true,
            _ => false,
        } {
            let msg = Owned(format!("Bad character {:?}", c));
            self.emit_error(msg);
        }

        debug!("got character {:?}", c);
        self.current_char = c;
        Some(c)
    }

    //§ tokenization
    // Get the next input character, if one is available.
    fn get_char(&mut self) -> Option<char> {
        if self.reconsume {
            self.reconsume = false;
            Some(self.current_char)
        } else {
            self.input_buffers.next()
                .and_then(|c| self.get_preprocessed_char(c))
        }
    }

    fn pop_except_from(&mut self, set: SmallCharSet) -> Option<SetResult> {
        // Bail to the slow path for various corner cases.
        // This means that `FromSet` can contain characters not in the set!
        // It shouldn't matter because the fallback `FromSet` case should
        // always do the same thing as the `NotFromSet` case.
        if self.opts.exact_errors || self.reconsume || self.ignore_lf {
            return self.get_char().map(|x| FromSet(x));
        }

        let d = self.input_buffers.pop_except_from(set);
        debug!("got characters {}", d);
        match d {
            Some(FromSet(c)) => self.get_preprocessed_char(c).map(|x| FromSet(x)),

            // NB: We don't set self.current_char for a run of characters not
            // in the set.  It shouldn't matter for the codepaths that use
            // this.
            _ => d
        }
    }

    // If fewer than n characters are available, return None.
    // Otherwise check if they satisfy a predicate, and consume iff so.
    //
    // FIXME: we shouldn't need to consume and then put back
    //
    // FIXME: do input stream preprocessing.  It's probably okay not to,
    // because none of the strings we look ahead for contain characters
    // affected by it, but think about this more.
    fn lookahead_and_consume(&mut self, n: uint, p: |&str| -> bool) -> Option<bool> {
        match self.input_buffers.pop_front(n) {
            None if self.at_eof => {
                debug!("lookahead: requested {:u} characters not available and never will be", n);
                Some(false)
            }
            None => {
                debug!("lookahead: requested {:u} characters not available", n);
                self.wait_for = Some(n);
                None
            }
            Some(s) => {
                if p(s.as_slice()) {
                    debug!("lookahead: condition satisfied by {:?}", s);
                    // FIXME: set current input character?
                    Some(true)
                } else {
                    debug!("lookahead: condition not satisfied by {:?}", s);
                    self.unconsume(s);
                    Some(false)
                }
            }
        }
    }

    // Run the state machine for as long as we can.
    fn run(&mut self) {
        if self.opts.profile {
            loop {
                let state = self.state;
                let old_sink = self.time_in_sink;
                let (run, mut dt) = time!(self.step());
                dt -= (self.time_in_sink - old_sink);
                self.state_profile.insert_or_update_with(state, dt, |_, x| *x += dt);
                if !run { break; }
            }
        } else {
            while self.step() {
            }
        }
    }

    fn bad_char_error(&mut self) {
        let msg = format_if!(
            self.opts.exact_errors,
            "Bad character",
            "Saw {:?} in state {:?}", self.current_char, self.state);
        self.emit_error(msg);
    }

    fn bad_eof_error(&mut self) {
        let msg = format_if!(
            self.opts.exact_errors,
            "Unexpected EOF",
            "Saw EOF in state {:?}", self.state);
        self.emit_error(msg);
    }

    fn emit_char(&mut self, c: char) {
        self.process_token(match c {
            '\0' => NullCharacterToken,
            _ => CharacterTokens(String::from_char(1, c)),
        });
    }

    // The string must not contain '\0'!
    fn emit_chars(&mut self, b: String) {
        self.process_token(CharacterTokens(b));
    }

    fn emit_current_tag(&mut self) {
        self.finish_attribute();

        let name = replace(&mut self.current_tag_name, String::new());
        let name = Atom::from_slice(name.as_slice());

        match self.current_tag_kind {
            StartTag => {
                self.last_start_tag_name = Some(name.clone());
            }
            EndTag => {
                if !self.current_tag_attrs.is_empty() {
                    self.emit_error(Slice("Attributes on an end tag"));
                }
                if self.current_tag_self_closing {
                    self.emit_error(Slice("Self-closing end tag"));
                }
            }
        }

        let token = TagToken(Tag { kind: self.current_tag_kind,
            name: name,
            self_closing: self.current_tag_self_closing,
            attrs: replace(&mut self.current_tag_attrs, vec!()),
        });
        self.process_token(token);

        if self.current_tag_kind == StartTag {
            match self.sink.query_state_change() {
                None => (),
                Some(s) => self.state = s,
            }
        }
    }

    fn emit_temp_buf(&mut self) {
        // FIXME: Make sure that clearing on emit is spec-compatible.
        let buf = replace(&mut self.temp_buf, empty_str());
        self.emit_chars(buf);
    }

    fn clear_temp_buf(&mut self) {
        // Do this without a new allocation.
        self.temp_buf.truncate(0);
    }

    fn emit_current_comment(&mut self) {
        let comment = replace(&mut self.current_comment, empty_str());
        self.process_token(CommentToken(comment));
    }

    fn discard_tag(&mut self) {
        self.current_tag_name = String::new();
        self.current_tag_self_closing = false;
        self.current_tag_attrs = vec!();
    }

    fn create_tag(&mut self, kind: TagKind, c: char) {
        self.discard_tag();
        self.current_tag_name.push_char(c);
        self.current_tag_kind = kind;
    }

    fn have_appropriate_end_tag(&self) -> bool {
        match self.last_start_tag_name.as_ref() {
            Some(last) =>
                (self.current_tag_kind == EndTag)
                && (self.current_tag_name.as_slice() == last.as_slice()),
            None => false,
        }
    }

    fn create_attribute(&mut self, c: char) {
        self.finish_attribute();

        self.current_attr_name.push_char(c);
    }

    fn finish_attribute(&mut self) {
        if self.current_attr_name.len() == 0 {
            return;
        }

        // Check for a duplicate attribute.
        // FIXME: the spec says we should error as soon as the name is finished.
        // FIXME: linear time search, do we care?
        let dup = {
            let name = self.current_attr_name.as_slice();
            self.current_tag_attrs.iter().any(|a| a.name.as_slice() == name)
        };

        if dup {
            self.emit_error(Slice("Duplicate attribute"));
            self.current_attr_name.truncate(0);
            self.current_attr_value.truncate(0);
        } else {
            let name = replace(&mut self.current_attr_name, String::new());
            self.current_tag_attrs.push(Attribute {
                name: AttrName::new(Atom::from_slice(name.as_slice())),
                value: replace(&mut self.current_attr_value, empty_str()),
            });
        }
    }

    fn emit_current_doctype(&mut self) {
        let doctype = replace(&mut self.current_doctype, Doctype::new());
        self.process_token(DoctypeToken(doctype));
    }

    fn doctype_id<'a>(&'a mut self, kind: DoctypeIdKind) -> &'a mut Option<String> {
        match kind {
            Public => &mut self.current_doctype.public_id,
            System => &mut self.current_doctype.system_id,
        }
    }

    fn clear_doctype_id(&mut self, kind: DoctypeIdKind) {
        let id = self.doctype_id(kind);
        match *id {
            Some(ref mut s) => s.truncate(0),
            None => *id = Some(empty_str()),
        }
    }

    fn consume_char_ref(&mut self, addnl_allowed: Option<char>) {
        // NB: The char ref tokenizer assumes we have an additional allowed
        // character iff we're tokenizing in an attribute value.
        self.char_ref_tokenizer = Some(box CharRefTokenizer::new(addnl_allowed));
    }

    fn emit_eof(&mut self) {
        self.process_token(EOFToken);
    }

    fn peek(&mut self) -> Option<char> {
        if self.reconsume {
            Some(self.current_char)
        } else {
            self.input_buffers.peek()
        }
    }

    fn discard_char(&mut self) {
        let c = self.get_char();
        assert!(c.is_some());
    }

    fn unconsume(&mut self, buf: String) {
        self.input_buffers.push_front(buf);
    }

    fn emit_error(&mut self, error: MaybeOwned<'static>) {
        self.process_token(ParseError(error));
    }
}
//§ END

// Shorthand for common state machine behaviors.
macro_rules! shorthand (
    ( $me:expr : emit $c:expr                    ) => ( $me.emit_char($c);                                   );
    ( $me:expr : create_tag $kind:expr $c:expr   ) => ( $me.create_tag($kind, $c);                           );
    ( $me:expr : push_tag $c:expr                ) => ( $me.current_tag_name.push_char($c);                  );
    ( $me:expr : discard_tag                     ) => ( $me.discard_tag();                                   );
    ( $me:expr : push_temp $c:expr               ) => ( $me.temp_buf.push_char($c);                          );
    ( $me:expr : emit_temp                       ) => ( $me.emit_temp_buf();                                 );
    ( $me:expr : clear_temp                      ) => ( $me.clear_temp_buf();                                );
    ( $me:expr : create_attr $c:expr             ) => ( $me.create_attribute($c);                            );
    ( $me:expr : push_name $c:expr               ) => ( $me.current_attr_name.push_char($c);                 );
    ( $me:expr : push_value $c:expr              ) => ( $me.current_attr_value.push_char($c);                );
    ( $me:expr : append_value $c:expr            ) => ( append_strings(&mut $me.current_attr_value, $c);     );
    ( $me:expr : push_comment $c:expr            ) => ( $me.current_comment.push_char($c);                   );
    ( $me:expr : append_comment $c:expr          ) => ( $me.current_comment.push_str($c);                    );
    ( $me:expr : emit_comment                    ) => ( $me.emit_current_comment();                          );
    ( $me:expr : clear_comment                   ) => ( $me.current_comment.truncate(0);                     );
    ( $me:expr : create_doctype                  ) => ( $me.current_doctype = Doctype::new();                );
    ( $me:expr : push_doctype_name $c:expr       ) => ( option_push_char(&mut $me.current_doctype.name, $c); );
    ( $me:expr : push_doctype_id $k:expr $c:expr ) => ( option_push_char($me.doctype_id($k), $c);            );
    ( $me:expr : clear_doctype_id $k:expr        ) => ( $me.clear_doctype_id($k);                            );
    ( $me:expr : force_quirks                    ) => ( $me.current_doctype.force_quirks = true;             );
    ( $me:expr : emit_doctype                    ) => ( $me.emit_current_doctype();                          );
    ( $me:expr : error                           ) => ( $me.bad_char_error();                                );
    ( $me:expr : error_eof                       ) => ( $me.bad_eof_error();                                 );
)

// Tracing of tokenizer actions.  This adds significant bloat and compile time,
// so it's behind a cfg flag.
#[cfg(trace_tokenizer)]
macro_rules! sh_trace ( ( $me:expr : $($cmds:tt)* ) => ({
    debug!("  {:s}", stringify!($($cmds)*));
    shorthand!($me:expr : $($cmds)*);
}))

#[cfg(not(trace_tokenizer))]
macro_rules! sh_trace ( ( $me:expr : $($cmds:tt)* ) => ( shorthand!($me: $($cmds)*) ) )

// A little DSL for sequencing shorthand actions.
macro_rules! go (
    // A pattern like $($cmd:tt)* ; $($rest:tt)* causes parse ambiguity.
    // We have to tell the parser how much lookahead we need.

    ( $me:expr : $a:tt                   ; $($rest:tt)* ) => ({ sh_trace!($me: $a);          go!($me: $($rest)*); });
    ( $me:expr : $a:tt $b:tt             ; $($rest:tt)* ) => ({ sh_trace!($me: $a $b);       go!($me: $($rest)*); });
    ( $me:expr : $a:tt $b:tt $c:tt       ; $($rest:tt)* ) => ({ sh_trace!($me: $a $b $c);    go!($me: $($rest)*); });
    ( $me:expr : $a:tt $b:tt $c:tt $d:tt ; $($rest:tt)* ) => ({ sh_trace!($me: $a $b $c $d); go!($me: $($rest)*); });

    // These can only come at the end.

    ( $me:expr : to $s:ident                   ) => ({ $me.state = states::$s; return true;           });
    ( $me:expr : to $s:ident $k1:expr          ) => ({ $me.state = states::$s($k1); return true;      });
    ( $me:expr : to $s:ident $k1:expr $k2:expr ) => ({ $me.state = states::$s($k1($k2)); return true; });

    ( $me:expr : reconsume $s:ident                   ) => ({ $me.reconsume = true; go!($me: to $s);         });
    ( $me:expr : reconsume $s:ident $k1:expr          ) => ({ $me.reconsume = true; go!($me: to $s $k1);     });
    ( $me:expr : reconsume $s:ident $k1:expr $k2:expr ) => ({ $me.reconsume = true; go!($me: to $s $k1 $k2); });

    ( $me:expr : consume_char_ref             ) => ({ $me.consume_char_ref(None); return true;         });
    ( $me:expr : consume_char_ref $addnl:expr ) => ({ $me.consume_char_ref(Some($addnl)); return true; });

    // We have a default next state after emitting a tag, but the sink can override.
    ( $me:expr : emit_tag $s:ident ) => ({
        $me.state = states::$s;
        $me.emit_current_tag();
        return true;
    });

    ( $me:expr : eof ) => ({ $me.emit_eof(); return false; });

    // If nothing else matched, it's a single command
    ( $me:expr : $($cmd:tt)+ ) => ( sh_trace!($me: $($cmd)+); );

    // or nothing.
    ($me:expr : ) => (());
)

macro_rules! go_match ( ( $me:expr : $x:expr, $($pats:pat)|+ => $($cmds:tt)* ) => (
    match $x {
        $($pats)|+ => go!($me: $($cmds)*),
        _ => (),
    }
))

// This is a macro because it can cause early return
// from the function where it is used.
macro_rules! get_char ( ($me:expr) => (
    unwrap_or_return!($me.get_char(), false)
))

macro_rules! pop_except_from ( ($me:expr, $set:expr) => (
    unwrap_or_return!($me.pop_except_from($set), false)
))

// NB: if you use this after get_char!(self) then the first char is still
// consumed no matter what!
macro_rules! lookahead_and_consume ( ($me:expr, $n:expr, $pred:expr) => (
    match $me.lookahead_and_consume($n, $pred) {
        // This counts as progress because we set the
        // wait_for variable.
        None => return true,
        Some(r) => r
    }
))

impl<'sink, Sink: TokenSink> Tokenizer<'sink, Sink> {
    // Run the state machine for a while.
    // Return true if we should be immediately re-invoked
    // (this just simplifies control flow vs. break / continue).
    fn step(&mut self) -> bool {
        if self.char_ref_tokenizer.is_some() {
            return self.step_char_ref_tokenizer();
        }

        match self.wait_for {
            Some(n) if !self.input_buffers.has(n) => {
                debug!("lookahead: requested {:u} characters still not available", n);
                return false;
            }
            Some(n) => {
                debug!("lookahead: requested {:u} characters become available", n);
                self.wait_for = None;
            }
            None => (),
        }

        debug!("processing in state {:?}", self.state);
        match self.state {
            //§ data-state
            states::Data => loop {
                match pop_except_from!(self, small_char_set!('\r' '\0' '&' '<')) {
                    FromSet('\0') => go!(self: error; emit '\0'),
                    FromSet('&')  => go!(self: consume_char_ref),
                    FromSet('<')  => go!(self: to TagOpen),
                    FromSet(c)    => go!(self: emit c),
                    NotFromSet(b) => self.emit_chars(b),
                }
            },

            //§ rcdata-state
            states::RawData(Rcdata) => loop {
                match pop_except_from!(self, small_char_set!('\r' '\0' '&' '<')) {
                    FromSet('\0') => go!(self: error; emit '\ufffd'),
                    FromSet('&') => go!(self: consume_char_ref),
                    FromSet('<') => go!(self: to RawLessThanSign Rcdata),
                    FromSet(c) => go!(self: emit c),
                    NotFromSet(b) => self.emit_chars(b),
                }
            },

            //§ rawtext-state
            states::RawData(Rawtext) => loop {
                match pop_except_from!(self, small_char_set!('\r' '\0' '<')) {
                    FromSet('\0') => go!(self: error; emit '\ufffd'),
                    FromSet('<') => go!(self: to RawLessThanSign Rawtext),
                    FromSet(c) => go!(self: emit c),
                    NotFromSet(b) => self.emit_chars(b),
                }
            },

            //§ script-data-state
            states::RawData(ScriptData) => loop {
                match pop_except_from!(self, small_char_set!('\r' '\0' '<')) {
                    FromSet('\0') => go!(self: error; emit '\ufffd'),
                    FromSet('<') => go!(self: to RawLessThanSign ScriptData),
                    FromSet(c) => go!(self: emit c),
                    NotFromSet(b) => self.emit_chars(b),
                }
            },

            //§ script-data-escaped-state
            states::RawData(ScriptDataEscaped(Escaped)) => loop {
                match pop_except_from!(self, small_char_set!('\r' '\0' '-' '<')) {
                    FromSet('\0') => go!(self: error; emit '\ufffd'),
                    FromSet('-') => go!(self: emit '-'; to ScriptDataEscapedDash Escaped),
                    FromSet('<') => go!(self: to RawLessThanSign ScriptDataEscaped Escaped),
                    FromSet(c) => go!(self: emit c),
                    NotFromSet(b) => self.emit_chars(b),
                }
            },

            //§ script-data-double-escaped-state
            states::RawData(ScriptDataEscaped(DoubleEscaped)) => loop {
                match pop_except_from!(self, small_char_set!('\r' '\0' '-' '<')) {
                    FromSet('\0') => go!(self: error; emit '\ufffd'),
                    FromSet('-') => go!(self: emit '-'; to ScriptDataEscapedDash DoubleEscaped),
                    FromSet('<') => go!(self: emit '<'; to RawLessThanSign ScriptDataEscaped DoubleEscaped),
                    FromSet(c) => go!(self: emit c),
                    NotFromSet(b) => self.emit_chars(b),
                }
            },

            //§ plaintext-state
            states::Plaintext => loop {
                match pop_except_from!(self, small_char_set!('\r' '\0')) {
                    FromSet('\0') => go!(self: error; emit '\ufffd'),
                    FromSet(c)    => go!(self: emit c),
                    NotFromSet(b) => self.emit_chars(b),
                }
            },

            //§ tag-open-state
            states::TagOpen => loop { match get_char!(self) {
                '!' => go!(self: to MarkupDeclarationOpen),
                '/' => go!(self: to EndTagOpen),
                '?' => go!(self: error; clear_comment; push_comment '?'; to BogusComment),
                c => match lower_ascii_letter(c) {
                    Some(cl) => go!(self: create_tag StartTag cl; to TagName),
                    None     => go!(self: error; emit '<'; reconsume Data),
                }
            }},

            //§ end-tag-open-state
            states::EndTagOpen => loop { match get_char!(self) {
                '>'  => go!(self: error; to Data),
                '\0' => go!(self: error; clear_comment; push_comment '\ufffd'; to BogusComment),
                c => match lower_ascii_letter(c) {
                    Some(cl) => go!(self: create_tag EndTag cl; to TagName),
                    None     => go!(self: error; clear_comment; push_comment c; to BogusComment),
                }
            }},

            //§ tag-name-state
            states::TagName => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' '
                     => go!(self: to BeforeAttributeName),
                '/'  => go!(self: to SelfClosingStartTag),
                '>'  => go!(self: emit_tag Data),
                '\0' => go!(self: error; push_tag '\ufffd'),
                c    => go!(self: push_tag (lower_ascii(c))),
            }},

            //§ script-data-escaped-less-than-sign-state
            states::RawLessThanSign(ScriptDataEscaped(Escaped)) => loop { match get_char!(self) {
                '/' => go!(self: clear_temp; to RawEndTagOpen ScriptDataEscaped Escaped),
                c => match lower_ascii_letter(c) {
                    Some(cl) => go!(self: clear_temp; push_temp cl; emit '<'; emit c;
                                    to ScriptDataEscapeStart DoubleEscaped),
                    None => go!(self: emit '<'; reconsume RawData ScriptDataEscaped Escaped),
                }
            }},

            //§ script-data-double-escaped-less-than-sign-state
            states::RawLessThanSign(ScriptDataEscaped(DoubleEscaped)) => loop { match get_char!(self) {
                '/' => go!(self: clear_temp; emit '/'; to ScriptDataDoubleEscapeEnd),
                _   => go!(self: reconsume RawData ScriptDataEscaped DoubleEscaped),
            }},

            //§ rcdata-less-than-sign-state rawtext-less-than-sign-state script-data-less-than-sign-state
            // otherwise
            states::RawLessThanSign(kind) => loop { match get_char!(self) {
                '/' => go!(self: clear_temp; to RawEndTagOpen kind),
                '!' if kind == ScriptData => go!(self: emit '<'; emit '!'; to ScriptDataEscapeStart Escaped),
                _   => go!(self: emit '<'; reconsume RawData kind),
            }},

            //§ rcdata-end-tag-open-state rawtext-end-tag-open-state script-data-end-tag-open-state script-data-escaped-end-tag-open-state
            states::RawEndTagOpen(kind) => loop {
                let c = get_char!(self);
                match lower_ascii_letter(c) {
                    Some(cl) => go!(self: create_tag EndTag cl; push_temp c; to RawEndTagName kind),
                    None     => go!(self: emit '<'; emit '/'; reconsume RawData kind),
                }
            },

            //§ rcdata-end-tag-name-state rawtext-end-tag-name-state script-data-end-tag-name-state script-data-escaped-end-tag-name-state
            states::RawEndTagName(kind) => loop {
                let c = get_char!(self);
                if self.have_appropriate_end_tag() {
                    match c {
                        '\t' | '\n' | '\x0C' | ' '
                            => go!(self: to BeforeAttributeName),
                        '/' => go!(self: to SelfClosingStartTag),
                        '>' => go!(self: emit_tag Data),
                        _ => (),
                    }
                }

                match lower_ascii_letter(c) {
                    Some(cl) => go!(self: push_tag cl; push_temp c),
                    None     => go!(self: discard_tag; emit '<'; emit '/'; emit_temp; reconsume RawData kind),
                }
            },

            //§ script-data-double-escape-start-state
            states::ScriptDataEscapeStart(DoubleEscaped) => loop {
                let c = get_char!(self);
                match c {
                    '\t' | '\n' | '\x0C' | ' ' | '/' | '>' => {
                        let esc = if self.temp_buf.as_slice() == "script" { DoubleEscaped } else { Escaped };
                        go!(self: emit c; to RawData ScriptDataEscaped esc);
                    }
                    _ => match lower_ascii_letter(c) {
                        Some(cl) => go!(self: push_temp cl; emit c),
                        None     => go!(self: reconsume RawData ScriptDataEscaped Escaped),
                    }
                }
            },

            //§ script-data-escape-start-state
            states::ScriptDataEscapeStart(Escaped) => loop { match get_char!(self) {
                '-' => go!(self: emit '-'; to ScriptDataEscapeStartDash),
                _   => go!(self: reconsume RawData ScriptData),
            }},

            //§ script-data-escape-start-dash-state
            states::ScriptDataEscapeStartDash => loop { match get_char!(self) {
                '-' => go!(self: emit '-'; to ScriptDataEscapedDashDash Escaped),
                _   => go!(self: reconsume RawData ScriptData),
            }},

            //§ script-data-escaped-dash-state script-data-double-escaped-dash-state
            states::ScriptDataEscapedDash(kind) => loop { match get_char!(self) {
                '-'  => go!(self: emit '-'; to ScriptDataEscapedDashDash kind),
                '<'  => {
                    if kind == DoubleEscaped { go!(self: emit '<'); }
                    go!(self: to RawLessThanSign ScriptDataEscaped kind);
                }
                '\0' => go!(self: error; emit '\ufffd'; to RawData ScriptDataEscaped kind),
                c    => go!(self: emit c; to RawData ScriptDataEscaped kind),
            }},

            //§ script-data-escaped-dash-dash-state script-data-double-escaped-dash-dash-state
            states::ScriptDataEscapedDashDash(kind) => loop { match get_char!(self) {
                '-'  => go!(self: emit '-'),
                '<'  => {
                    if kind == DoubleEscaped { go!(self: emit '<'); }
                    go!(self: to RawLessThanSign ScriptDataEscaped kind);
                }
                '>'  => go!(self: emit '>'; to RawData ScriptData),
                '\0' => go!(self: error; emit '\ufffd'; to RawData ScriptDataEscaped kind),
                c    => go!(self: emit c; to RawData ScriptDataEscaped kind),
            }},

            //§ script-data-double-escape-end-state
            states::ScriptDataDoubleEscapeEnd => loop {
                let c = get_char!(self);
                match c {
                    '\t' | '\n' | '\x0C' | ' ' | '/' | '>' => {
                        let esc = if self.temp_buf.as_slice() == "script" { Escaped } else { DoubleEscaped };
                        go!(self: emit c; to RawData ScriptDataEscaped esc);
                    }
                    _ => match lower_ascii_letter(c) {
                        Some(cl) => go!(self: push_temp cl; emit c),
                        None     => go!(self: reconsume RawData ScriptDataEscaped DoubleEscaped),
                    }
                }
            },

            //§ before-attribute-name-state
            states::BeforeAttributeName => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' ' => (),
                '/'  => go!(self: to SelfClosingStartTag),
                '>'  => go!(self: emit_tag Data),
                '\0' => go!(self: error; create_attr '\ufffd'; to AttributeName),
                c    => match lower_ascii_letter(c) {
                    Some(cl) => go!(self: create_attr cl; to AttributeName),
                    None => {
                        go_match!(self: c,
                            '"' | '\'' | '<' | '=' => error);
                        go!(self: create_attr c; to AttributeName);
                    }
                }
            }},

            //§ attribute-name-state
            states::AttributeName => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' '
                     => go!(self: to AfterAttributeName),
                '/'  => go!(self: to SelfClosingStartTag),
                '='  => go!(self: to BeforeAttributeValue),
                '>'  => go!(self: emit_tag Data),
                '\0' => go!(self: error; push_name '\ufffd'),
                c    => match lower_ascii_letter(c) {
                    Some(cl) => go!(self: push_name cl),
                    None => {
                        go_match!(self: c,
                            '"' | '\'' | '<' => error);
                        go!(self: push_name c);
                    }
                }
            }},

            //§ after-attribute-name-state
            states::AfterAttributeName => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' ' => (),
                '/'  => go!(self: to SelfClosingStartTag),
                '='  => go!(self: to BeforeAttributeValue),
                '>'  => go!(self: emit_tag Data),
                '\0' => go!(self: error; create_attr '\ufffd'; to AttributeName),
                c    => match lower_ascii_letter(c) {
                    Some(cl) => go!(self: create_attr cl; to AttributeName),
                    None => {
                        go_match!(self: c,
                            '"' | '\'' | '<' => error);
                        go!(self: create_attr c; to AttributeName);
                    }
                }
            }},

            //§ before-attribute-value-state
            states::BeforeAttributeValue => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' ' => (),
                '"'  => go!(self: to AttributeValue DoubleQuoted),
                '&'  => go!(self: reconsume AttributeValue Unquoted),
                '\'' => go!(self: to AttributeValue SingleQuoted),
                '\0' => go!(self: error; push_value '\ufffd'; to AttributeValue Unquoted),
                '>'  => go!(self: error; emit_tag Data),
                c => {
                    go_match!(self: c,
                        '<' | '=' | '`' => error);
                    go!(self: push_value c; to AttributeValue Unquoted);
                }
            }},

            //§ attribute-value-(double-quoted)-state
            states::AttributeValue(DoubleQuoted) => loop {
                match pop_except_from!(self, small_char_set!('\r' '"' '&' '\0')) {
                    FromSet('"')  => go!(self: to AfterAttributeValueQuoted),
                    FromSet('&')  => go!(self: consume_char_ref '"'),
                    FromSet('\0') => go!(self: error; push_value '\ufffd'),
                    FromSet(c)    => go!(self: push_value c),
                    NotFromSet(b) => go!(self: append_value b),
                }
            },

            //§ attribute-value-(single-quoted)-state
            states::AttributeValue(SingleQuoted) => loop {
                match pop_except_from!(self, small_char_set!('\r' '\'' '&' '\0')) {
                    FromSet('\'') => go!(self: to AfterAttributeValueQuoted),
                    FromSet('&')  => go!(self: consume_char_ref '\''),
                    FromSet('\0') => go!(self: error; push_value '\ufffd'),
                    FromSet(c)    => go!(self: push_value c),
                    NotFromSet(b) => go!(self: append_value b),
                }
            },

            //§ attribute-value-(unquoted)-state
            states::AttributeValue(Unquoted) => loop {
                match pop_except_from!(self, small_char_set!('\r' '\t' '\n' '\x0C' ' ' '&' '>' '\0')) {
                    FromSet('\t') | FromSet('\n') | FromSet('\x0C') | FromSet(' ')
                     => go!(self: to BeforeAttributeName),
                    FromSet('&')  => go!(self: consume_char_ref '>'),
                    FromSet('>')  => go!(self: emit_tag Data),
                    FromSet('\0') => go!(self: error; push_value '\ufffd'),
                    FromSet(c) => {
                        go_match!(self: c,
                            '"' | '\'' | '<' | '=' | '`' => error);
                        go!(self: push_value c);
                    }
                    NotFromSet(b) => go!(self: append_value b),
                }
            },

            //§ after-attribute-value-(quoted)-state
            states::AfterAttributeValueQuoted => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' '
                     => go!(self: to BeforeAttributeName),
                '/'  => go!(self: to SelfClosingStartTag),
                '>'  => go!(self: emit_tag Data),
                _    => go!(self: error; reconsume BeforeAttributeName),
            }},

            //§ self-closing-start-tag-state
            states::SelfClosingStartTag => loop { match get_char!(self) {
                '>' => {
                    self.current_tag_self_closing = true;
                    go!(self: emit_tag Data);
                }
                _ => go!(self: error; reconsume BeforeAttributeName),
            }},

            //§ comment-start-state
            states::CommentStart => loop { match get_char!(self) {
                '-'  => go!(self: to CommentStartDash),
                '\0' => go!(self: error; push_comment '\ufffd'; to Comment),
                '>'  => go!(self: error; emit_comment; to Data),
                c    => go!(self: push_comment c; to Comment),
            }},

            //§ comment-start-dash-state
            states::CommentStartDash => loop { match get_char!(self) {
                '-'  => go!(self: to CommentEnd),
                '\0' => go!(self: error; append_comment "-\ufffd"; to Comment),
                '>'  => go!(self: error; emit_comment; to Data),
                c    => go!(self: push_comment '-'; push_comment c; to Comment),
            }},

            //§ comment-state
            states::Comment => loop { match get_char!(self) {
                '-'  => go!(self: to CommentEndDash),
                '\0' => go!(self: error; push_comment '\ufffd'),
                c    => go!(self: push_comment c),
            }},

            //§ comment-end-dash-state
            states::CommentEndDash => loop { match get_char!(self) {
                '-'  => go!(self: to CommentEnd),
                '\0' => go!(self: error; append_comment "-\ufffd"; to Comment),
                c    => go!(self: push_comment '-'; push_comment c; to Comment),
            }},

            //§ comment-end-state
            states::CommentEnd => loop { match get_char!(self) {
                '>'  => go!(self: emit_comment; to Data),
                '\0' => go!(self: error; append_comment "--\ufffd"; to Comment),
                '!'  => go!(self: error; to CommentEndBang),
                '-'  => go!(self: error; push_comment '-'),
                c    => go!(self: error; append_comment "--"; push_comment c; to Comment),
            }},

            //§ comment-end-bang-state
            states::CommentEndBang => loop { match get_char!(self) {
                '-'  => go!(self: append_comment "--!"; to CommentEndDash),
                '>'  => go!(self: emit_comment; to Data),
                '\0' => go!(self: error; append_comment "--!\ufffd"; to Comment),
                c    => go!(self: append_comment "--!"; push_comment c; to Comment),
            }},

            //§ doctype-state
            states::Doctype => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' '
                    => go!(self: to BeforeDoctypeName),
                _   => go!(self: error; reconsume BeforeDoctypeName),
            }},

            //§ before-doctype-name-state
            states::BeforeDoctypeName => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' ' => (),
                '\0' => go!(self: error; create_doctype; push_doctype_name '\ufffd'; to DoctypeName),
                '>'  => go!(self: error; create_doctype; force_quirks; emit_doctype; to Data),
                c    => go!(self: create_doctype; push_doctype_name (lower_ascii(c)); to DoctypeName),
            }},

            //§ doctype-name-state
            states::DoctypeName => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' '
                     => go!(self: to AfterDoctypeName),
                '>'  => go!(self: emit_doctype; to Data),
                '\0' => go!(self: error; push_doctype_name '\ufffd'),
                c    => go!(self: push_doctype_name (lower_ascii(c))),
            }},

            //§ after-doctype-name-state
            states::AfterDoctypeName => loop {
                if lookahead_and_consume!(self, 6, |s| s.eq_ignore_ascii_case("public")) {
                    go!(self: to AfterDoctypeKeyword Public);
                } else if lookahead_and_consume!(self, 6, |s| s.eq_ignore_ascii_case("system")) {
                    go!(self: to AfterDoctypeKeyword System);
                } else {
                    match get_char!(self) {
                        '\t' | '\n' | '\x0C' | ' ' => (),
                        '>' => go!(self: emit_doctype; to Data),
                        _   => go!(self: error; force_quirks; to BogusDoctype),
                    }
                }
            },

            //§ after-doctype-public-keyword-state after-doctype-system-keyword-state
            states::AfterDoctypeKeyword(kind) => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' '
                     => go!(self: to BeforeDoctypeIdentifier kind),
                '"'  => go!(self: error; clear_doctype_id kind; to DoctypeIdentifierDoubleQuoted kind),
                '\'' => go!(self: error; clear_doctype_id kind; to DoctypeIdentifierSingleQuoted kind),
                '>'  => go!(self: error; force_quirks; emit_doctype; to Data),
                _    => go!(self: error; force_quirks; to BogusDoctype),
            }},

            //§ before-doctype-public-identifier-state before-doctype-system-identifier-state
            states::BeforeDoctypeIdentifier(kind) => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' ' => (),
                '"'  => go!(self: clear_doctype_id kind; to DoctypeIdentifierDoubleQuoted kind),
                '\'' => go!(self: clear_doctype_id kind; to DoctypeIdentifierSingleQuoted kind),
                '>'  => go!(self: error; force_quirks; emit_doctype; to Data),
                _    => go!(self: error; force_quirks; to BogusDoctype),
            }},

            //§ doctype-public-identifier-(double-quoted)-state doctype-system-identifier-(double-quoted)-state
            states::DoctypeIdentifierDoubleQuoted(kind) => loop { match get_char!(self) {
                '"'  => go!(self: to AfterDoctypeIdentifier kind),
                '\0' => go!(self: error; push_doctype_id kind '\ufffd'),
                '>'  => go!(self: error; force_quirks; emit_doctype; to Data),
                c    => go!(self: push_doctype_id kind c),
            }},

            //§ doctype-public-identifier-(single-quoted)-state doctype-system-identifier-(single-quoted)-state
            states::DoctypeIdentifierSingleQuoted(kind) => loop { match get_char!(self) {
                '\'' => go!(self: to AfterDoctypeIdentifier kind),
                '\0' => go!(self: error; push_doctype_id kind '\ufffd'),
                '>'  => go!(self: error; force_quirks; emit_doctype; to Data),
                c    => go!(self: push_doctype_id kind c),
            }},

            //§ after-doctype-public-identifier-state
            states::AfterDoctypeIdentifier(Public) => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' '
                     => go!(self: to BetweenDoctypePublicAndSystemIdentifiers),
                '>'  => go!(self: emit_doctype; to Data),
                '"'  => go!(self: error; clear_doctype_id System; to DoctypeIdentifierDoubleQuoted System),
                '\'' => go!(self: error; clear_doctype_id System; to DoctypeIdentifierSingleQuoted System),
                _    => go!(self: error; force_quirks; to BogusDoctype),
            }},

            //§ after-doctype-system-identifier-state
            states::AfterDoctypeIdentifier(System) => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' ' => (),
                '>' => go!(self: emit_doctype; to Data),
                _   => go!(self: error; to BogusDoctype),
            }},

            //§ between-doctype-public-and-system-identifiers-state
            states::BetweenDoctypePublicAndSystemIdentifiers => loop { match get_char!(self) {
                '\t' | '\n' | '\x0C' | ' ' => (),
                '>'  => go!(self: emit_doctype; to Data),
                '"'  => go!(self: clear_doctype_id System; to DoctypeIdentifierDoubleQuoted System),
                '\'' => go!(self: clear_doctype_id System; to DoctypeIdentifierSingleQuoted System),
                _    => go!(self: error; force_quirks; to BogusDoctype),
            }},

            //§ bogus-doctype-state
            states::BogusDoctype => loop { match get_char!(self) {
                '>'  => go!(self: emit_doctype; to Data),
                _    => (),
            }},

            //§ bogus-comment-state
            states::BogusComment => loop { match get_char!(self) {
                '>'  => go!(self: emit_comment; to Data),
                '\0' => go!(self: push_comment '\ufffd'),
                c    => go!(self: push_comment c),
            }},

            //§ markup-declaration-open-state
            states::MarkupDeclarationOpen => loop {
                if lookahead_and_consume!(self, 2, |s| s == "--") {
                    go!(self: clear_comment; to CommentStart);
                } else if lookahead_and_consume!(self, 7, |s| s.eq_ignore_ascii_case("doctype")) {
                    go!(self: to Doctype);
                } else {
                    // FIXME: CDATA, requires "adjusted current node" from tree builder
                    // FIXME: 'error' gives wrong message
                    go!(self: error; to BogusComment);
                }
            },

            //§ cdata-section-state
            states::CdataSection
                => fail!("FIXME: state {:?} not implemented", self.state),
            //§ END
        }
    }

    fn step_char_ref_tokenizer(&mut self) -> bool {
        // FIXME HACK: Take and replace the tokenizer so we don't
        // double-mut-borrow self.  This is why it's boxed.
        let mut tok = self.char_ref_tokenizer.take_unwrap();
        let outcome = tok.step(self);

        let progress = match outcome {
            char_ref::Done => {
                self.process_char_ref(tok.get_result());
                return true;
            }

            char_ref::Stuck => false,
            char_ref::Progress => true,
        };

        self.char_ref_tokenizer = Some(tok);
        progress
    }

    fn process_char_ref(&mut self, char_ref: CharRef) {
        let CharRef { mut chars, mut num_chars } = char_ref;

        if num_chars == 0 {
            chars[0] = '&';
            num_chars = 1;
        }

        for i in range(0, num_chars) {
            let c = chars[i as uint];
            match self.state {
                states::Data | states::RawData(states::Rcdata)
                    => go!(self: emit c),

                states::AttributeValue(_)
                    => go!(self: push_value c),

                _ => fail!("state {:?} should not be reachable in process_char_ref", self.state),
            }
        }
    }

    /// Indicate that we have reached the end of the input.
    pub fn end(&mut self) {
        // Handle EOF in the char ref sub-tokenizer, if there is one.
        // Do this first because it might un-consume stuff.
        match self.char_ref_tokenizer.take() {
            None => (),
            Some(mut tok) => {
                tok.end_of_file(self);
                self.process_char_ref(tok.get_result());
            }
        }

        // Process all remaining buffered input.
        // If we're waiting for lookahead, we're not gonna get it.
        self.wait_for = None;
        self.at_eof = true;
        self.run();

        while self.eof_step() {
            // loop
        }

        if self.opts.profile {
            let mut results: Vec<(states::State, u64)>
                = self.state_profile.iter().map(|(s, t)| (*s, *t)).collect();
            results.sort_by(|&(_, x), &(_, y)| y.cmp(&x));

            let total = results.iter().map(|&(_, t)| t).sum();
            println!("\nTokenizer profile, in nanoseconds");
            println!("\n{:12u}         total in token sink", self.time_in_sink);
            println!("\n{:12u}         total in tokenizer", total);

            for (k, v) in results.move_iter() {
                let pct = 100.0 * (v as f64) / (total as f64);
                println!("{:12u}  {:4.1f}%  {:?}", v, pct, k);
            }
        }
    }

    fn eof_step(&mut self) -> bool {
        debug!("processing EOF in state {:?}", self.state);
        match self.state {
            states::Data | states::RawData(Rcdata) | states::RawData(Rawtext)
            | states::RawData(ScriptData) | states::Plaintext
                => go!(self: eof),

            states::TagName | states::RawData(ScriptDataEscaped(_))
            | states::BeforeAttributeName | states::AttributeName
            | states::AfterAttributeName | states::BeforeAttributeValue
            | states::AttributeValue(_) | states::AfterAttributeValueQuoted
            | states::SelfClosingStartTag | states::ScriptDataEscapedDash(_)
            | states::ScriptDataEscapedDashDash(_)
                => go!(self: error_eof; to Data),

            states::TagOpen
                => go!(self: error_eof; emit '<'; to Data),

            states::EndTagOpen
                => go!(self: error_eof; emit '<'; emit '/'; to Data),

            states::RawLessThanSign(ScriptDataEscaped(DoubleEscaped))
                => go!(self: to RawData ScriptDataEscaped DoubleEscaped),

            states::RawLessThanSign(kind)
                => go!(self: emit '<'; to RawData kind),

            states::RawEndTagOpen(kind)
                => go!(self: emit '<'; emit '/'; to RawData kind),

            states::RawEndTagName(kind)
                => go!(self: emit '<'; emit '/'; emit_temp; to RawData kind),

            states::ScriptDataEscapeStart(kind)
                => go!(self: to RawData ScriptDataEscaped kind),

            states::ScriptDataEscapeStartDash
                => go!(self: to RawData ScriptData),

            states::ScriptDataDoubleEscapeEnd
                => go!(self: to RawData ScriptDataEscaped DoubleEscaped),

            states::CommentStart | states::CommentStartDash
            | states::Comment | states::CommentEndDash
            | states::CommentEnd | states::CommentEndBang
                => go!(self: error_eof; emit_comment; to Data),

            states::Doctype | states::BeforeDoctypeName
                => go!(self: error_eof; create_doctype; force_quirks; emit_doctype; to Data),

            states::DoctypeName | states::AfterDoctypeName | states::AfterDoctypeKeyword(_)
            | states::BeforeDoctypeIdentifier(_) | states::DoctypeIdentifierDoubleQuoted(_)
            | states::DoctypeIdentifierSingleQuoted(_) | states::AfterDoctypeIdentifier(_)
            | states::BetweenDoctypePublicAndSystemIdentifiers
                => go!(self: error_eof; force_quirks; emit_doctype; to Data),

            states::BogusDoctype
                => go!(self: emit_doctype; to Data),

            states::BogusComment
                => go!(self: emit_comment; to Data),

            states::MarkupDeclarationOpen
                => go!(self: error; to BogusComment),

            states::CdataSection
                => fail!("FIXME: state {:?} not implemented in EOF", self.state),
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case_functions)]
mod test {
    use super::{option_push_char, append_strings}; // private items

    #[test]
    fn push_to_None_gives_singleton() {
        let mut s: Option<String> = None;
        option_push_char(&mut s, 'x');
        assert_eq!(s, Some("x".to_string()));
    }

    #[test]
    fn push_to_empty_appends() {
        let mut s: Option<String> = Some(String::new());
        option_push_char(&mut s, 'x');
        assert_eq!(s, Some("x".to_string()));
    }

    #[test]
    fn push_to_nonempty_appends() {
        let mut s: Option<String> = Some("y".to_string());
        option_push_char(&mut s, 'x');
        assert_eq!(s, Some("yx".to_string()));
    }

    #[test]
    fn append_appends() {
        let mut s = "foo".to_string();
        append_strings(&mut s, "bar".to_string());
        assert_eq!(s, "foobar".to_string());
    }

    #[test]
    fn append_to_empty_does_not_copy() {
        let mut lhs: String = "".to_string();
        let rhs: Vec<u8> = Vec::from_slice(b"foo");
        let ptr_old = rhs[0] as *const u8;

        append_strings(&mut lhs, String::from_utf8(rhs).unwrap());
        assert_eq!(lhs, "foo".to_string());

        let ptr_new = lhs.into_bytes()[0] as *const u8;
        assert_eq!(ptr_old, ptr_new);
    }
}
