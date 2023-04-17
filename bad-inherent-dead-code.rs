#![feature(inherent_associated_types)]
#![allow(incomplete_features)]

fn main() {
//    let _: Struct::Item = ();
}

struct Struct;
impl Struct { type Item = (); }
