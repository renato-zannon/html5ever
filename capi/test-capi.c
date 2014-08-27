// Copyright 2014 The html5ever Project Developers. See the
// COPYRIGHT file at the top-level directory of this distribution.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

#include "html5ever.h"

struct h5e_token_ops ops = {};

struct h5e_token_sink sink = {
    .ops = &ops,
    .user = NULL
};

int main() {
    h5e_tokenizer_new(&sink);
    return 0;
}
