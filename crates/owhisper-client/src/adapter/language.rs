#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum LanguageQuality {
    #[default]
    NoData,
    Moderate,
    Good,
    High,
    Excellent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageSupport {
    NotSupported,
    Supported { quality: LanguageQuality },
}

impl LanguageSupport {
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Supported { .. })
    }

    pub fn quality(&self) -> Option<LanguageQuality> {
        match self {
            Self::Supported { quality } => Some(*quality),
            Self::NotSupported => None,
        }
    }

    pub fn min(iter: impl IntoIterator<Item = Self>) -> Self {
        iter.into_iter()
            .reduce(|a, b| match (&a, &b) {
                (Self::NotSupported, _) | (_, Self::NotSupported) => Self::NotSupported,
                (Self::Supported { quality: q1 }, Self::Supported { quality: q2 }) => {
                    Self::Supported {
                        quality: (*q1).min(*q2),
                    }
                }
            })
            .unwrap_or(Self::Supported {
                quality: LanguageQuality::NoData,
            })
    }
}

impl PartialOrd for LanguageSupport {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for LanguageSupport {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::NotSupported, Self::NotSupported) => std::cmp::Ordering::Equal,
            (Self::NotSupported, Self::Supported { .. }) => std::cmp::Ordering::Less,
            (Self::Supported { .. }, Self::NotSupported) => std::cmp::Ordering::Greater,
            (Self::Supported { quality: q1 }, Self::Supported { quality: q2 }) => q1.cmp(q2),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LanguageQuality, LanguageSupport};

    #[test]
    fn min_of_empty_iter_is_supported_with_no_data() {
        assert_eq!(
            LanguageSupport::min(std::iter::empty()),
            LanguageSupport::Supported {
                quality: LanguageQuality::NoData,
            }
        );
    }
}

/// The language to send to providers that accept one language hint. With two
/// or more spoken languages (for example Arabic + English in one call), no
/// hint is sent, so the provider auto-detects and handles code-switching.
pub(crate) fn pinned_language(
    languages: &[anlg_language::Language],
) -> Option<&anlg_language::Language> {
    match languages {
        [language] => Some(language),
        _ => None,
    }
}

#[cfg(test)]
mod pinned_language_tests {
    use super::pinned_language;
    use anlg_language::ISO639;

    #[test]
    fn pins_only_a_single_language() {
        let one: Vec<anlg_language::Language> = vec![ISO639::Ar.into()];
        let two: Vec<anlg_language::Language> = vec![ISO639::Ar.into(), ISO639::En.into()];
        assert_eq!(pinned_language(&one).map(|l| l.iso639().code()), Some("ar"));
        assert!(pinned_language(&two).is_none());
        assert!(pinned_language(&[]).is_none());
    }
}
