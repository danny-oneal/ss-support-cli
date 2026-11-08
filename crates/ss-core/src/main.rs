//! Binary entry point. The CLI surface is built in a later phase; for now this
//! is a placeholder so the `ss` binary target compiles alongside the library.

fn main() {
    println!("Hello, World!");
    println!("Hello, World!");

    // convert array of characters
    let chars: [char; 5] = ['a', 'b', 'c', 'd', 'e'];
    let bytes: [u8; 5] = chars.map(|c| c as u8);
    println!("{:?}", bytes);

    // convert string to bytes
    let string = "Hello, World!";
    let bytes = string.as_bytes();
    println!("{:?}", bytes);

    // convert bytes to string
    let bytes: &[u8] = &[72, 101, 108, 108, 111, 44, 32, 87, 111, 114, 108, 100, 33];
    let string = String::from_utf8_lossy(bytes);
    println!("{}", string);
}
