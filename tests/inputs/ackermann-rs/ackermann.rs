#![no_std]
#![feature(start)]

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

// cannot use #![no_main] due to Fatal error "Unknown start function: `$main`" (I suspect something in the wasm assembler)
#[start]
pub fn main(_argc: isize, _argv: *const *const u8) -> isize { 0 }

#[no_mangle]
pub fn ackermann(m: u32, n: u32) -> u32 {
  if m == 0 {
    n + 1
  } else if n == 0 {
    ackermann(m - 1, 1)
  } else {
    ackermann(m - 1, ackermann(m, n - 1))
  }
}
