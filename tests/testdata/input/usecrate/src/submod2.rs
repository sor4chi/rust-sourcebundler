use crate::submod1;
// full-line comment: should be preserved in the bundle
pub fn hello_world2() {
    submod1::hello_world1(); // via `use crate::submod1`
    crate::submod1::hello_world1(); // inline crate:: path
    println!("Hello, world 2!");
}
