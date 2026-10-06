//! OWNS: the float mode every audio callback runs in: flush-to-zero and denormals-are-zero on
//! (MXCSR bits 15 and 6), as Blink's audio thread ran the Web Audio code lf-engine ports, and as hosts
//! run plugins. A decaying tail would otherwise sit in subnormals, which many CPUs compute far slower,
//! inside the callback's deadline; a lane delay's feedback above 0.5 never even reaches 0 without it
//! (its tail rounds to a few subnormal steps and stays there). The dev PC's Zen 4 showed no cost
//! either way (the lane delays ringing out under the Stage 3 effects, 2026-10-07).
//!
//! Per callback, not once per thread: the driver's thread gets its mode back on return, and a plugin
//! or driver that changes the mode cannot leave it changed past one callback. lf-engine's offline
//! renders (tests, the export) run without it; they differ only below the subnormal threshold.

/// The callback's float mode, restored on drop. Two instructions each way: no alloc, lock or syscall.
pub(crate) struct DenormalsOff {
    #[cfg(target_arch = "x86_64")]
    saved: u32,
}

#[cfg(target_arch = "x86_64")]
const FTZ_DAZ: u32 = 1 << 15 | 1 << 6;

impl DenormalsOff {
    #[must_use = "the mode holds only while the guard lives"]
    pub(crate) fn new() -> DenormalsOff {
        #[cfg(target_arch = "x86_64")]
        {
            let saved = mxcsr::read();
            mxcsr::write(saved | FTZ_DAZ);
            DenormalsOff { saved }
        }
        #[cfg(not(target_arch = "x86_64"))]
        DenormalsOff {}
    }
}

impl Drop for DenormalsOff {
    fn drop(&mut self) {
        #[cfg(target_arch = "x86_64")]
        mxcsr::write(self.saved);
    }
}

/// `_mm_getcsr`/`_mm_setcsr` are deprecated: inline assembly is what the standard library points to.
#[cfg(target_arch = "x86_64")]
mod mxcsr {
    use std::arch::asm;

    pub(super) fn read() -> u32 {
        let mut csr = 0u32;
        // SAFETY: stores the 32-bit MXCSR into a local the pointer names.
        unsafe { asm!("stmxcsr [{}]", in(reg) &mut csr, options(nostack, preserves_flags)) };
        csr
    }

    pub(super) fn write(csr: u32) {
        // SAFETY: loads MXCSR from a local; callers pass a value read from MXCSR with only FTZ and DAZ
        // changed, so no reserved bit is set (which would fault).
        unsafe { asm!("ldmxcsr [{}]", in(reg) &csr, options(nostack, preserves_flags, readonly)) };
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod tests {
    use super::*;
    use std::hint::black_box;

    /// MXCSR without its six sticky status flags, which any float operation may raise.
    fn mode() -> u32 {
        mxcsr::read() & !0x3f
    }

    #[test]
    fn inside_the_guard_subnormals_flush_and_after_it_the_mode_is_back() {
        let before = mode();
        // The smallest normal halved is subnormal; the smallest subnormal times 2^24 is normal again, so
        // only DAZ (not FTZ, which acts on results) can make that product 0.
        let (normal, subnormal, lift) = (f32::MIN_POSITIVE, f32::from_bits(1), 16_777_216.0f32);
        let ftz = || black_box(normal) * black_box(0.5f32);
        let daz = || black_box(subnormal) * black_box(lift);
        // 2^-127 (subnormal) and 2^-125 (normal), by their bits.
        let kept = (f32::from_bits(1 << 22), f32::from_bits(2 << 23));
        assert_eq!((ftz(), daz()), kept, "the default mode keeps subnormals");
        {
            let _guard = DenormalsOff::new();
            assert_eq!(mxcsr::read() & FTZ_DAZ, FTZ_DAZ);
            assert_eq!(ftz(), 0.0, "FTZ: a subnormal result is written as 0");
            assert_eq!(daz(), 0.0, "DAZ: a subnormal operand is read as 0");
        }
        assert_eq!(mode(), before);
        assert_eq!((ftz(), daz()), kept);
    }

    #[test]
    fn a_guard_restores_the_mode_it_found_not_the_default() {
        let before = mode();
        let outer = DenormalsOff::new();
        drop(DenormalsOff::new());
        assert_eq!(mxcsr::read() & FTZ_DAZ, FTZ_DAZ, "the inner guard turned the outer's mode off");
        drop(outer);
        assert_eq!(mode(), before);
    }
}
