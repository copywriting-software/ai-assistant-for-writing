/// Deterministic keyboard-layout & common encoding repair utilities.
///
/// Helps instantly fix text typed on the wrong keyboard layout (e.g. `ghbdtn` -> `привет`,
/// or `руддщ` -> `hello`) without waiting for an LLM roundtrip, and can also be paired with
/// AI grammar fixing.

const EN_LAYOUT: &str = "qwertyuiop[]asdfghjkl;'zxcvbnm,.`QWERTYUIOP{}ASDFGHJKL:\"ZXCVBNM<>~@#$^&/?";
const RU_LAYOUT: &str = "йцукенгшщзхъфывапролджэячсмитьбюёЙЦУКЕНГШЩЗХЪФЫВАПРОЛДЖЭЯЧСМИТЬБЮЁ\"№;:?.,";

pub fn convert_en_to_ru_layout(input: &str) -> String {
    let en_chars: Vec<char> = EN_LAYOUT.chars().collect();
    let ru_chars: Vec<char> = RU_LAYOUT.chars().collect();
    input
        .chars()
        .map(|ch| {
            en_chars
                .iter()
                .position(|&c| c == ch)
                .and_then(|idx| ru_chars.get(idx).copied())
                .unwrap_or(ch)
        })
        .collect()
}

pub fn convert_ru_to_en_layout(input: &str) -> String {
    let en_chars: Vec<char> = EN_LAYOUT.chars().collect();
    let ru_chars: Vec<char> = RU_LAYOUT.chars().collect();
    input
        .chars()
        .map(|ch| {
            ru_chars
                .iter()
                .position(|&c| c == ch)
                .and_then(|idx| en_chars.get(idx).copied())
                .unwrap_or(ch)
        })
        .collect()
}

/// Heuristically checks whether a single word or short phrase looks like Russian typed
/// on an English QWERTY layout (e.g. `ghbdtn`, `rfr ltkf`, `cjplfnm`).
pub fn looks_like_russian_in_en_layout(input: &str) -> bool {
    let trimmed = input.trim();
    if trimmed.len() < 3 {
        return false;
    }
    // Must contain mostly ASCII letters/punctuation that map to Cyrillic, and no existing Cyrillic.
    if trimmed.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)) {
        return false;
    }

    let converted = convert_en_to_ru_layout(trimmed);
    let cyrillic_count = converted
        .chars()
        .filter(|c| ('\u{0400}'..='\u{04FF}').contains(c))
        .count();
    if cyrillic_count < 3 {
        return false;
    }

    // Check if the input contains classic Russian-in-EN-layout markers like `[`, `]`, `;`, `'`, `,` inside words
    // or lacks English vowels in long words while having Russian vowels after conversion.
    let words: Vec<&str> = trimmed.split_whitespace().collect();
    let mut suspicious_words = 0usize;
    for w in &words {
        let lower = w.to_lowercase();
        let has_en_vowel = lower.chars().any(|c| matches!(c, 'a' | 'e' | 'i' | 'o' | 'u' | 'y'));
        let has_punct_inside = lower.contains('[') || lower.contains(']') || lower.contains(';') || lower.contains('\'');
        let conv_w = convert_en_to_ru_layout(&lower);
        let has_ru_vowel = conv_w
            .chars()
            .any(|c| matches!(c, 'а' | 'е' | 'ё' | 'и' | 'о' | 'у' | 'ы' | 'э' | 'ю' | 'я'));

        if has_punct_inside && has_ru_vowel {
            suspicious_words += 1;
        } else if lower.len() >= 3 && !has_en_vowel && has_ru_vowel {
            suspicious_words += 1;
        }
    }

    suspicious_words > 0 && suspicious_words * 2 >= words.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_en_to_ru_layout() {
        assert_eq!(convert_en_to_ru_layout("Ghbdtn? rfr ltkf&"), "Привет, как дела?");
        assert_eq!(convert_en_to_ru_layout("ghbdtn"), "привет");
    }

    #[test]
    fn converts_ru_to_en_layout() {
        assert_eq!(convert_ru_to_en_layout("руддщ цщкдв"), "hello world");
    }

    #[test]
    fn detects_russian_in_en_layout_without_false_positives_on_english() {
        assert!(looks_like_russian_in_en_layout("ghbdtn rfr ltkf"));
        assert!(!looks_like_russian_in_en_layout("hello how are you"));
    }
}
