//! nano ID — secure, URL-safe, collision-resistant unique IDs (a Rust port of the
//! JavaScript [`nanoid`](https://github.com/ai/nanoid)).
//!
//! Collision resistance rests on three properties, all preserved here:
//!
//! 1. **Cryptographic randomness** — every byte comes from the OS CSPRNG
//!    (`getrandom`), never a PRNG seeded from time.
//! 2. **Uniform sampling (no modulo bias)** — the default 64-character alphabet
//!    is a power of two, so `byte & 63` maps each value to exactly one symbol
//!    with no bias. Custom alphabets use masked **rejection sampling** so every
//!    symbol stays equally likely.
//! 3. **Sufficient entropy** — the default length 21 over a 64-symbol alphabet is
//!    `21 * log2(64) = 126` bits, exceeding `UUIDv4`'s 122 bits. At 1000 IDs/sec
//!    for ~150 years the collision probability stays below one in a billion.
//!
//! `unsafe` is not used; randomness comes from `getrandom`'s safe API.

/// The URL-safe 64-symbol alphabet used by `nanoid` (`A-Za-z0-9_-`, scrambled).
const URL_ALPHABET: &[u8; 64] = b"useandom-26T198340PX75pxJACKVERYMINDBUSHWOLF_GQZbfghjklqvwyzrict";

/// Default ID length (≈126 bits of entropy).
pub const DEFAULT_SIZE: usize = 21;

/// Fill `buffer` from the OS CSPRNG.
///
/// # Panics
/// Panics only if the OS has no usable entropy source — an unrecoverable
/// condition for any ID generator (mirrors `uuid`'s behavior).
fn fill(buffer: &mut [u8]) {
    getrandom::fill(buffer).expect("OS CSPRNG unavailable");
}

/// Generate a default 21-character nano ID.
#[must_use]
pub fn nanoid() -> String {
    nanoid_sized(DEFAULT_SIZE)
}

/// Generate a nano ID of `size` characters from the URL-safe alphabet.
///
/// Uses the bias-free fast path (`byte & 63`); a small stack buffer avoids a
/// heap allocation for typical sizes.
#[must_use]
pub fn nanoid_sized(size: usize) -> String {
    let mut bytes = smallvec::SmallVec::<[u8; 32]>::from_elem(0, size);
    fill(&mut bytes);
    bytes
        .iter()
        .map(|byte| URL_ALPHABET[(byte & 63) as usize] as char)
        .collect()
}

/// Generate a `size`-character ID from a custom `alphabet`, using uniform
/// masked rejection sampling (no modulo bias → collision properties hold).
///
/// # Panics
/// Panics if `alphabet` is empty or longer than 255 symbols.
#[must_use]
pub fn nanoid_custom(size: usize, alphabet: &[char]) -> String {
    let len = alphabet.len();
    assert!(
        (1..=255).contains(&len),
        "alphabet must have 1..=255 symbols"
    );
    if size == 0 {
        return String::new();
    }

    // mask = smallest (2^k - 1) >= len-1 — same formula as JS nanoid, computed
    // with integer math (no float casts, no modulo bias).
    let mask = (2_usize << ((len - 1) | 1).ilog2()) - 1;
    // Over-read factor 1.6 = 8/5 keeps the expected number of CSPRNG calls ~1.
    // Saturating math hardens against overflow on pathologically large sizes.
    let step = 8_usize
        .saturating_mul(mask)
        .saturating_mul(size)
        .div_ceil(5 * len)
        .max(1);

    let mut id = String::with_capacity(size);
    let mut chunk = smallvec::SmallVec::<[u8; 64]>::from_elem(0, step);
    loop {
        fill(&mut chunk);
        for &byte in &chunk {
            let index = byte as usize & mask;
            if index < len {
                id.push(alphabet[index]);
                if id.len() == size {
                    return id;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_SIZE, URL_ALPHABET, nanoid, nanoid_custom, nanoid_sized};
    use std::collections::HashSet;

    fn is_url_char(character: char) -> bool {
        URL_ALPHABET.contains(&(character as u8))
    }

    #[test]
    fn default_has_expected_length() {
        assert_eq!(nanoid().chars().count(), DEFAULT_SIZE);
    }

    #[test]
    fn sized_has_expected_length() {
        for size in [0, 1, 8, 21, 40, 100] {
            assert_eq!(nanoid_sized(size).chars().count(), size);
        }
    }

    #[test]
    fn only_url_safe_characters() {
        let id = nanoid_sized(256);
        assert!(id.chars().all(is_url_char), "unexpected character in {id}");
    }

    #[test]
    fn no_collisions_over_many_ids() {
        let mut seen = HashSet::new();
        for _ in 0..50_000 {
            assert!(seen.insert(nanoid()), "collision generated");
        }
    }

    #[test]
    fn custom_alphabet_only_uses_given_symbols() {
        let alphabet: Vec<char> = "abcdef".chars().collect();
        let id = nanoid_custom(64, &alphabet);
        assert_eq!(id.chars().count(), 64);
        assert!(id.chars().all(|character| alphabet.contains(&character)));
    }

    #[test]
    fn custom_alphabet_covers_all_symbols() {
        // A 10-symbol alphabet (non-power-of-two) exercises rejection sampling.
        let alphabet: Vec<char> = "0123456789".chars().collect();
        let id = nanoid_custom(5_000, &alphabet);
        let used: HashSet<char> = id.chars().collect();
        assert_eq!(used.len(), alphabet.len(), "not all symbols appeared");
    }

    #[test]
    fn custom_size_zero_is_empty() {
        assert!(nanoid_custom(0, &['a', 'b']).is_empty());
    }
}
