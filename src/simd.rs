//! Core SIMD implementation.

use core::fmt;
use std::arch::x86_64::__m256i;
use std::arch::x86_64::_mm256_and_si256;
use std::arch::x86_64::_mm256_andnot_si256;
use std::arch::x86_64::_mm256_blend_epi16;
use std::arch::x86_64::_mm256_blendv_epi8;
use std::arch::x86_64::_mm256_or_si256;
use std::arch::x86_64::_mm256_permutevar8x32_epi32;
use std::arch::x86_64::_mm256_set1_epi32;
use std::arch::x86_64::_mm256_set_epi32;
use std::arch::x86_64::_mm256_set_epi8;
use std::arch::x86_64::_mm256_setr_epi32;
use std::arch::x86_64::_mm256_shuffle_epi8;
use std::arch::x86_64::_mm256_slli_epi32;
use std::arch::x86_64::_mm256_srli_epi32;
use std::simd::prelude::*;
use std::simd::LaneCount;
use std::simd::SimdElement;
use std::simd::SupportedLaneCount;
use std::simd::ToBytes;

use crate::util::invert_index;
use crate::util::tiled;

/// Decodes `ascii` as base64. Returns the results of the decoding in the low
/// 3/4 of the returned vector. `invalid` is made non-zero if decoding fails.
#[inline]
pub fn decode<const N: usize>(ascii: Simd<u8, N>, invalid: &mut Simd<u8, N>) -> Simd<u8, N>
where
  LaneCount<N>: SupportedLaneCount,
  LaneCount<{N/4}>: SupportedLaneCount,
  Simd<u32, {N/4}>: ToBytes<Bytes = Simd<u8, N>>,
  __m256i: From<Simd<u8, N>>,
  Simd<u8, N>: From<__m256i>,
{
  // We need to convert each ASCII octet into a sextet, according to this match:
  //
  //    match c {
  //      A..=Z => c - 'A',       // c - 65 in 0x41..=0x5a
  //      a..=z => c - 'a' + 26,  // c - 71 in 0x61..=0x7a
  //      0..=9 => c - '0' + 52,  // c + 4  in 0x30..=0x39
  //      +     => c - '+' + 62,  // c + 19 in 0x2b
  //      /     => c - '/' + 63,  // c + 16 in 0x2f
  //    }

  // One approach is to use comparison masks to extract the pieces of the
  // input corresponding to each of the five cases above, and then map them
  // to the corresponding value we need to offset `ascii` by.

  /*
  use std::ops::RangeInclusive;
  let in_range = |bytes: Simd<u8, N>, range: RangeInclusive<u8>| {
    bytes.simd_ge(Simd::splat(*range.start()))
      & bytes.simd_le(Simd::splat(*range.end()))
  };

  let uppers = in_range(ascii, b'A'..=b'Z');
  let lowers = in_range(ascii, b'a'..=b'z');
  let digits = in_range(ascii, b'0'..=b'9');
  let pluses = ascii.simd_eq(Simd::splat(b'+'));
  let slashes = ascii.simd_eq(Simd::splat(b'/'));

  let valid = (uppers | lowers | digits | pluses | slashes).all();

  let sextets = ascii.cast::<i8>()
    + mask_splat(uppers, -65)
    + mask_splat(lowers, -71)
    + mask_splat(digits, 4)
    + mask_splat(pluses, 19)
    + mask_splat(slashes, 16);
  */

  // However, it turns out to be *almost twice as fast* to use a perfect hash!
  //
  // The function `|c| (c >> 4) - (c == '/')` is a perfect hash for
  // the match above, which maps the five categories as such:
  //
  //    match c {
  //      A..=Z => 4 or 5,
  //      a..=z => 6 or 7,
  //      0..=9 => 3,
  //      +     => 2,
  //      /     => 1,
  //    }
  //
  // We can then use a shuffle to select one of the corresponding offsets,
  // -65, -71, 4, 19, or 16, and add that to `ascii`.
  //
  // This perfect hash function is described at
  // https://github.com/WojciechMula/base64simd/issues/3.

  let hashes = (ascii >> Simd::splat(4))
    + Simd::simd_eq(ascii, Simd::splat(b'/'))
      .to_int()
      .cast::<u8>();

  let sextets =
    ascii + tiled(&[!0, 16, 19, 4, 191, 191, 185, 185]).swizzle_dyn(hashes);

  // We also need to do a range check to reject invalid characters.

  const LO_LUT: Simd<u8, 16> = Simd::from_array([
    0b10101, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001,
    0b10001, 0b10001, 0b10011, 0b11010, 0b11011, 0b11011, 0b11011, 0b11010,
  ]);

  const HI_LUT: Simd<u8, 16> = Simd::from_array([
    0b10000, 0b10000, 0b00001, 0b00010, 0b00100, 0b01000, 0b00100, 0b01000,
    0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000,
  ]);

  let lo = swizzle::<16, N>(LO_LUT, ascii & Simd::splat(0x0f));
  let hi = swizzle::<16, N>(HI_LUT, ascii >> Simd::splat(4));
  *invalid |= lo & hi;

  // Now we need to shift everything a little bit, since each byte has two high
  // bits it shouldn't that we need to delete. One thing we can do is to split
  // the vector into two alternating vectors, convert them to vectors of u16,
  // shift each lane by a specified amount, and then shuffle-OR them back
  // together. I learned this trick from Danila Kutenin.
  //
  // What we're basically going to do is the following. Below letters represent
  // the decoded message and dots are extraneous zeros. (Bits below are ordered
  // little-endian.)
  //
  // start:
  //  Aaaaaa.. Bbbbbb.. Cccccc.. Dddddd.. eeeeee.. ffffff.. gggggg.. hhhhhh..
  //  ..aaaaaa ..bbbbbb ..cccccc ..dddddd ..eeeeee ..ffffff ..gggggg ..hhhhhh
  //  ........ aaaaaabb bbbbcccc ccdddddd
  // 00000000 aaaaaabb bbbbcccc ccdddddd
  // zext to u16:
  //  aaaaaa.......... bbbbbb.......... cccccc.......... dddddd..........
  //  eeeeee.......... ffffff.......... gggggg.......... hhhhhh..........
  //
  // u16 shift:
  //  ..aaaaaa|........ ....bbbb|bb...... ......cc|cccc.... ........|dddddd..
  //  ..eeeeee|........ ....ffff|ff...... ......gg|gggg.... ........|hhhhhh..
  //
  // u16 d3einterleave:
  //  ..aaaaaa ....bbbb ......cc ........ ..eeeeee ....ffff ......gg ........
  //  ........ bb...... cccc.... dddddd.. ........ ff...... gggg.... hhhhhh..
  //
  // u8 rotate:
  //  ..aaaaaa ....bbbb ......cc ........ ..eeeeee ....ffff ......gg ........
  //  bb...... cccc.... dddddd.. ........ ff...... gggg.... hhhhhh.. ........
  //
  // u8 or:
  //  bbaaaaaa ccccbbbb ddddddcc ........ ffeeeeee ggggffff hhhhhhgg ........
  //
  // u8 shuffle:
  //  bbaaaaaa ccccbbbb ddddddcc ffeeeeee ggggffff hhhhhhgg ........ ........

  let blocks: __m256i = sextets.into();
  let shift_d = unsafe { _mm256_srli_epi32::<24>(blocks) };
  let shift_c = unsafe { _mm256_srli_epi32::<10>(blocks) };
  let shift_b = unsafe { _mm256_slli_epi32::<4>(blocks) };
  let shift_a = unsafe { _mm256_slli_epi32::<18>(blocks) };
  let shift_ac = unsafe { _mm256_blend_epi16::<0b10101010>(shift_c, shift_a) };
  let shift_bd = unsafe { _mm256_blendv_epi8(shift_d, shift_b, _mm256_set1_epi32(0x00ffff00)) };
  let mask = unsafe { _mm256_set1_epi32(0x00fc0fc0) };
  let decoded_chunks = unsafe { _mm256_or_si256(_mm256_and_si256(mask, shift_ac), _mm256_andnot_si256(mask, shift_bd)) };

  let intralane = unsafe { _mm256_shuffle_epi8(decoded_chunks, Simd::from_array([2i8, 1, 0, 6, 5, 4, 10, 9, 8, 14, 13, 12, -1, -1, -1, -1, 2, 1, 0, 6, 5, 4, 10, 9, 8, 14, 13, 12, -1, -1, -1, -1]).into()) };
  let interlane = unsafe { _mm256_permutevar8x32_epi32(intralane, Simd::from_array([0i32, 1, 2, 4, 5, 6, 3, 7]).into()) };

  interlane.into()
}

/// Encodes the low 3/4 bytes of `data` as base64. The high quarter of the
/// input is ignored.
#[inline]
pub fn encode<const N: usize>(data: Simd<u8, N>) -> Simd<u8, N>
where
  LaneCount<N>: SupportedLaneCount,
{
  // First, insert some extra zeros every third lane.
  let data = swizzle!(N; data, invert_index(array!(N; |i| i + i / 3)));

  // Next, we need to undo the "or" at the end of decode_simd.
  let mask = tiled(&[0b11111100, 0b11110000, 0b11000000, 0b00000000]);

  // Note that we also need to undo the rotate we did to `hi`.
  let lo = data & mask;
  let hi = (data & !mask).rotate_elements_right::<1>();

  // Interleave the shuffled pieces and undo the shift.
  let shifted = lo.cast::<u16>() | (hi.cast::<u16>() << Simd::splat(8));
  let sextets = (shifted >> tiled(&[2, 4, 6, 8])).cast::<u8>();

  // Now we have what is essentially a u6 array that looks like this:
  //  aaaaaa.. bbbbbb.. cccccc.. dddddd.. eeeeee.. ffffff.. gggggg.. hhhhhh..

  // We need to split into five ranges: 0x00..=0x19, 0x1a..=0x33, 0x34..=0x3d,
  // 0x3e, and 0x3f. If we (saturating) subtract 0x1a from each range, we get
  //
  // - 0x00..=0x0f
  // - 0x10..=0x29
  // - 0x2a..=0x33
  // - 0x34,  0x35
  //
  // If we then form a mask from "sextets >= 0x34", and add the low nybble of
  // the mask (effectively, adding 0xf to the bottom two rows) we get
  //
  // - 0x00..=0x0f
  // - 0x10..=0x29
  // - 0x39..=0x42
  // - 0x43, =0x44
  //
  // Then, if we form a mask from "sextets >= 0x3e", select 0x1c, and add that
  // to the result, we get
  //
  // - 0x00..=0x0f
  // - 0x10..=0x29
  // - 0x39..=0x42
  // - 0x5f, =0x60
  //
  // If we shift the high nybbles down, this contrivance is a perfect hash, just
  // like in the encoding function.

  let hashes = (sextets.saturating_sub(Simd::splat(0x0a))
    + mask_splat(sextets.simd_ge(Simd::splat(0x34)), 0x0f)
    + mask_splat(sextets.simd_ge(Simd::splat(0x3e)), 0x1c))
    >> Simd::splat(4);

  let offsets = tiled(&[191, 185, 185, 4, 4, 19, 16, !0]).swizzle_dyn(hashes);

  sextets - offsets
}

/// Shorthand for mask.select(splat(val), splat(0)).
fn mask_splat<T, const N: usize>(mask: Mask<T::Mask, N>, val: T) -> Simd<T, N>
where
  T: SimdElement + Default,
  LaneCount<N>: SupportedLaneCount,
{
  mask.select(Simd::splat(val), Simd::splat(Default::default()))
}

/// Resizes a vector by either truncation or padding with zeroes.
fn resize<T, const N: usize, const M: usize>(v: Simd<T, N>) -> Simd<T, M>
where
  T: SimdElement + Default,
  LaneCount<N>: SupportedLaneCount,
  LaneCount<M>: SupportedLaneCount,
{
  let len = usize::min(N, M);
  let mut out = Simd::default();
  out.as_mut_array()[..len].copy_from_slice(&v.as_array()[..len]);
  out
}

/// Creates a new `M`-byte vector by treating each element of `indices` as an
/// index into `table`, which is treated as being padded to infinite length
/// with zero.
fn swizzle<const N: usize, const M: usize>(
  table: Simd<u8, N>,
  indices: Simd<u8, M>,
) -> Simd<u8, M>
where
  LaneCount<N>: SupportedLaneCount,
  LaneCount<M>: SupportedLaneCount,
{
  if N < M {
    Simd::swizzle_dyn(resize(table), indices)
  } else {
    resize(Simd::swizzle_dyn(table, resize(indices)))
  }
}

// Helper for debug printing vectors.
#[allow(dead_code)]
struct SimdDbg<V>(pub V);

impl<T, const N: usize> fmt::Binary for SimdDbg<Simd<T, N>>
where
  T: SimdElement + fmt::Binary,
  LaneCount<N>: SupportedLaneCount,
{
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    struct Patch<T>(T);
    impl<T: fmt::Binary> fmt::Debug for Patch<T> {
      fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Binary::fmt(&self.0, f)
      }
    }

    let mut f = f.debug_list();
    for b in self.0.to_array() {
      f.entry(&Patch(b));
    }
    f.finish()
  }
}

impl<T, const N: usize> fmt::LowerHex for SimdDbg<Simd<T, N>>
where
  T: SimdElement + fmt::LowerHex,
  LaneCount<N>: SupportedLaneCount,
{
  fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
    struct Patch<T>(T);
    impl<T: fmt::LowerHex> fmt::Debug for Patch<T> {
      fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::LowerHex::fmt(&self.0, f)
      }
    }

    let mut f = f.debug_list();
    for b in self.0.to_array() {
      f.entry(&Patch(b));
    }
    f.finish()
  }
}
