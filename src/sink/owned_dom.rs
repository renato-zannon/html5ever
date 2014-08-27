// Copyright 2014 The html5ever Project Developers. See the
// COPYRIGHT file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A simple DOM where every node is owned by its parent.
//!
//! Since ownership is more complicated during parsing, we actually
//! build a different type and then transmute to the public `Node`.
//! This is believed to be memory safe, but if you want to be extra
//! careful you can use `RcDom` instead.

use sink::common::{NodeEnum, Document, Doctype, Text, Comment, Element};

use util::namespace::{Namespace, HTML};
use tokenizer::Attribute;
use tree_builder::{TreeSink, QuirksMode, NodeOrText, AppendNode, AppendText};
use tree_builder;
use serialize::{Serializable, Serializer};
use driver::ParseResult;

use std::ty::Unsafe;
use std::default::Default;
use std::io::IoResult;
use std::mem::transmute;
use std::kinds::marker;
use std::collections::HashSet;
use std::mem;
use std::str::MaybeOwned;

use string_cache::Atom;

/// The internal type we use for nodes during parsing.
struct SquishyNode {
    node: NodeEnum,
    parent: Handle,
    children: Vec<Handle>,
}

impl SquishyNode {
    fn new(node: NodeEnum) -> SquishyNode {
        SquishyNode {
            node: node,
            parent: Handle::null(),
            children: vec!(),
        }
    }
}

struct Handle {
    ptr: *const Unsafe<SquishyNode>,
    no_send: marker::NoSend,
    no_sync: marker::NoSync,
}

impl Handle {
    fn new(ptr: *const Unsafe<SquishyNode>) -> Handle {
        Handle {
            ptr: ptr,
            no_send: marker::NoSend,
            no_sync: marker::NoSync,
        }
    }

    fn null() -> Handle {
        Handle::new(RawPtr::null())
    }

    fn is_null(&self) -> bool {
        self.ptr.is_null()
    }
}

impl PartialEq for Handle {
    fn eq(&self, other: &Handle) -> bool {
        self.ptr == other.ptr
    }
}

impl Eq for Handle { }

impl Clone for Handle {
    fn clone(&self) -> Handle {
        Handle::new(self.ptr)
    }
}

// The safety of `Deref` and `DerefMut` depends on the invariant that `Handle`s
// can't escape the `Sink`, because nodes are deallocated by consuming the
// `Sink`.

impl DerefMut<SquishyNode> for Handle {
    fn deref_mut<'a>(&'a mut self) -> &'a mut SquishyNode {
        unsafe {
            transmute::<_, &'a mut SquishyNode>((*self.ptr).get())
        }
    }
}

impl Deref<SquishyNode> for Handle {
    fn deref<'a>(&'a self) -> &'a SquishyNode {
        unsafe {
            transmute::<_, &'a SquishyNode>((*self.ptr).get())
        }
    }
}

fn append(mut new_parent: Handle, mut child: Handle) {
    new_parent.children.push(child);
    let parent = &mut child.parent;
    assert!(parent.is_null());
    *parent = new_parent
}

fn get_parent_and_index(mut child: Handle) -> Option<(Handle, uint)> {
    if child.parent.is_null() {
        return None;
    }

    let to_find = child;
    match child.parent.children.iter().enumerate().find(|&(_, n)| *n == to_find) {
        Some((i, _)) => Some((child.parent, i)),
        None => fail!("have parent but couldn't find in parent's children!"),
    }
}

fn append_to_existing_text(mut prev: Handle, text: &str) -> bool {
    match prev.deref_mut().node {
        Text(ref mut existing) => {
            existing.push_str(text);
            true
        }
        _ => false,
    }
}

pub struct Sink {
    nodes: Vec<Box<Unsafe<SquishyNode>>>,
    document: Handle,
    errors: Vec<MaybeOwned<'static>>,
    quirks_mode: QuirksMode,
}

impl Default for Sink {
    fn default() -> Sink {
        let mut sink = Sink {
            nodes: vec!(),
            document: Handle::null(),
            errors: vec!(),
            quirks_mode: tree_builder::NoQuirks,
        };
        sink.document = sink.new_node(Document);
        sink
    }
}

impl Sink {
    fn new_node(&mut self, node: NodeEnum) -> Handle {
        self.nodes.push(box Unsafe::new(SquishyNode::new(node)));
        let ptr: *const Unsafe<SquishyNode> = &**self.nodes.last().unwrap();
        Handle::new(ptr)
    }
}

impl TreeSink<Handle> for Sink {
    fn parse_error(&mut self, msg: MaybeOwned<'static>) {
        self.errors.push(msg);
    }

    fn get_document(&mut self) -> Handle {
        self.document
    }

    fn set_quirks_mode(&mut self, mode: QuirksMode) {
        self.quirks_mode = mode;
    }

    fn same_node(&self, x: Handle, y: Handle) -> bool {
        x == y
    }

    fn elem_name(&self, target: Handle) -> (Namespace, Atom) {
        match target.node {
            Element(ref name, _) => (HTML, name.clone()),
            _ => fail!("not an element!"),
        }
    }

    fn create_element(&mut self, ns: Namespace, name: Atom, attrs: Vec<Attribute>) -> Handle {
        assert!(ns == HTML);
        self.new_node(Element(name, attrs))
    }

    fn create_comment(&mut self, text: String) -> Handle {
        self.new_node(Comment(text))
    }

    fn append(&mut self, mut parent: Handle, child: NodeOrText<Handle>) {
        // Append to an existing Text node if we have one.
        match child {
            AppendText(ref text) => match parent.children.last() {
                Some(h) => if append_to_existing_text(*h, text.as_slice()) { return; },
                _ => (),
            },
            _ => (),
        }

        append(parent, match child {
            AppendText(text) => self.new_node(Text(text)),
            AppendNode(node) => node
        });
    }

    fn append_before_sibling(&mut self,
            sibling: Handle,
            child: NodeOrText<Handle>) -> Result<(), NodeOrText<Handle>> {
        let (mut parent, i) = unwrap_or_return!(get_parent_and_index(sibling), Err(child));

        let mut child = match (child, i) {
            // No previous node.
            (AppendText(text), 0) => self.new_node(Text(text)),

            // Look for a text node before the insertion point.
            (AppendText(text), i) => {
                let prev = parent.children[i-1];
                if append_to_existing_text(prev, text.as_slice()) {
                    return Ok(());
                }
                self.new_node(Text(text))
            }

            // The tree builder promises we won't have a text node after
            // the insertion point.

            // Any other kind of node.
            (AppendNode(node), _) => node,
        };

        if !child.parent.is_null() {
            self.remove_from_parent(child);
        }

        child.parent = parent;
        parent.children.insert(i, child);
        Ok(())
    }

    fn append_doctype_to_document(&mut self, name: String, public_id: String, system_id: String) {
        append(self.document, self.new_node(Doctype(name, public_id, system_id)));
    }

    fn add_attrs_if_missing(&mut self, mut target: Handle, mut attrs: Vec<Attribute>) {
        let existing = match target.deref_mut().node {
            Element(_, ref mut attrs) => attrs,
            _ => return,
        };

        // FIXME: quadratic time
        attrs.retain(|attr|
            !existing.iter().any(|e| e.name == attr.name));
        existing.push_all_move(attrs);
    }

    fn remove_from_parent(&mut self, mut target: Handle) {
        let (mut parent, i) = unwrap_or_return!(get_parent_and_index(target), ());
        parent.children.remove(i).expect("not found!");
        target.parent = Handle::null();
    }

    fn mark_script_already_started(&mut self, _node: Handle) { }
}

pub struct Node {
    pub node: NodeEnum,
    _parent_not_accessible: uint,
    pub children: Vec<Box<Node>>,
}

pub struct OwnedDom {
    pub document: Box<Node>,
    pub errors: Vec<MaybeOwned<'static>>,
    pub quirks_mode: QuirksMode,
}

impl ParseResult<Sink> for OwnedDom {
    fn get_result(sink: Sink) -> OwnedDom {
        fn walk(live: &mut HashSet<uint>, node: Handle) {
            live.insert(node.ptr as uint);
            for &child in node.deref().children.iter() {
                walk(live, child);
            }
        }

        // Collect addresses of all the nodes that made it into the final tree.
        let mut live = HashSet::new();
        walk(&mut live, sink.document);

        // Forget about the nodes in the final tree; they will be owned by
        // their parent.  In the process of iterating we drop all nodes that
        // aren't in the tree.
        for node in sink.nodes.move_iter() {
            let ptr: *const Unsafe<SquishyNode> = &*node;
            if live.contains(&(ptr as uint)) {
                unsafe {
                    mem::forget(node);
                }
            }
        }

        let old_addrs = addrs_of!(sink.document: node, parent, children);

        // Transmute the root to a Node, finalizing the transfer of ownership.
        let document = unsafe {
            mem::transmute::<*const Unsafe<SquishyNode>, Box<Node>>(sink.document.ptr)
        };

        // FIXME: do this assertion statically
        let new_addrs = addrs_of!(document: node, _parent_not_accessible, children);
        assert_eq!(old_addrs, new_addrs);

        OwnedDom {
            document: document,
            errors: sink.errors,
            quirks_mode: sink.quirks_mode,
        }
    }
}

impl Serializable for Node {
    fn serialize<'wr, Wr: Writer>(&self,
            serializer: &mut Serializer<'wr, Wr>,
            incl_self: bool) -> IoResult<()> {

        match (incl_self, &self.node) {
            (_, &Element(ref name, ref attrs)) => {
                if incl_self {
                    try!(serializer.start_elem(HTML, name.clone(),
                        attrs.iter().map(|at| (&at.name, at.value.as_slice()))));
                }

                for child in self.children.iter() {
                    try!(child.serialize(serializer, true));
                }

                if incl_self {
                    try!(serializer.end_elem(HTML, name.clone()));
                }
                Ok(())
            }

            (false, &Document) => {
                for child in self.children.iter() {
                    try!(child.serialize(serializer, true));
                }
                Ok(())
            }

            (false, _) => Ok(()),

            (true, &Doctype(ref name, _, _)) => serializer.write_doctype(name.as_slice()),
            (true, &Text(ref text)) => serializer.write_text(text.as_slice()),
            (true, &Comment(ref text)) => serializer.write_comment(text.as_slice()),

            (true, &Document) => fail!("Can't serialize Document node itself"),
        }
    }
}
