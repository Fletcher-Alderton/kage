//! Unified thinking effort shared by providers, the loop, and frontends,
//! and the thinking settings a model accepts.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Unified thinking effort the host requests for one turn.
///
/// An eight-step ladder the user cycles through with `Shift+Tab`. [`Reasoning::resolve`] fits a level to
/// what the model accepts, and providers translate it to their shape
/// (an effort value, a thinking token budget, or an on/off switch).
///
/// The `Off` variant means thinking is disabled: providers send the
/// model's way of switching thinking off, or no thinking field.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// Thinking disabled, or the lowest level for a model that cannot
    /// turn it off.
    #[default]
    Off,
    /// Smallest budget the model accepts (`OpenAI` `reasoning_effort=minimal`).
    Minimal,
    /// Light reasoning. `OpenAI` maps to `reasoning_effort=low`.
    Low,
    /// Moderate reasoning. `OpenAI` maps to `reasoning_effort=medium`.
    Medium,
    /// Heavier reasoning. `OpenAI` maps to `reasoning_effort=high`.
    High,
    /// Heavier than high. `OpenAI` keeps `high` on a model without an
    /// effort list; budget-based providers allocate a large tier.
    #[serde(rename = "xhigh")]
    XHigh,
    /// The model's `max` effort, or the largest budget tier.
    Max,
    /// The `ultra` effort above `max`. Budget-based models stop at
    /// [`Self::Max`].
    Ultra,
}

impl ThinkingLevel {
    /// Every level, lowest first.
    pub const ALL: [Self; 8] = [
        Self::Off,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
        Self::Ultra,
    ];

    /// The level after `current` in `levels` (lowest first), wrapping to
    /// the first. `None` when `levels` is empty. Used by the TUI's
    /// `Shift+Tab` rotation so it only visits levels the model accepts.
    #[must_use]
    pub fn next_in(levels: &[Self], current: Option<Self>) -> Option<Self> {
        let first = levels.first().copied()?;
        Some(
            current
                .and_then(|c| levels.iter().copied().find(|l| *l > c))
                .unwrap_or(first),
        )
    }

    /// Short, lowercase label for the modeline pill. The `Off`
    /// variant returns `"off"`; callers that want to suppress the
    /// pill entirely should check [`Self::is_off`] first.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }

    /// Wire-stable kebab-case identifier used in plugin event payloads
    /// and session-entry records. Mirrors the [`serde`] rename so
    /// `ThinkingLevel::High.as_str() == "high"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        self.label()
    }

    /// Parse a wire identifier produced by [`Self::as_str`]. Returns
    /// `None` for unrecognized values; callers that want a strict
    /// fallback should use `unwrap_or(Self::Off)`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::XHigh),
            "max" => Some(Self::Max),
            "ultra" => Some(Self::Ultra),
            _ => None,
        }
    }

    /// `true` when thinking is disabled. Providers should use this to
    /// decide whether to omit the thinking-related body field.
    #[must_use]
    pub fn is_off(self) -> bool {
        matches!(self, Self::Off)
    }

    /// Map the level to an `OpenAI` `reasoning_effort` string for a
    /// model without a catalog effort list. Returns `None` for
    /// [`Self::Off`] (caller should omit the field). `XHigh`, `Max`
    /// and `Ultra` collapse to `"high"`, the largest value every such
    /// model takes.
    #[must_use]
    pub fn openai_reasoning_effort(self) -> Option<&'static str> {
        match self {
            Self::Off => None,
            Self::Minimal => Some("minimal"),
            Self::Low => Some("low"),
            Self::Medium => Some("medium"),
            Self::High | Self::XHigh | Self::Max | Self::Ultra => Some("high"),
        }
    }

    /// Budget in thinking tokens for budget-based providers
    /// (Anthropic, Gemini), before [`Reasoning::budget`] fits it to the
    /// model's bounds. Returns `None` for [`Self::Off`].
    #[must_use]
    pub fn default_budget_tokens(self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::Minimal => Some(1_024),
            Self::Low => Some(4_096),
            Self::Medium => Some(8_192),
            Self::High => Some(16_384),
            Self::XHigh => Some(32_768),
            Self::Max | Self::Ultra => Some(65_536),
        }
    }
}

/// One reasoning effort value a model accepts on the wire, as models.dev
/// lists it.
///
/// Efforts map onto the [`ThinkingLevel`] ladder by name. `none` is
/// [`ThinkingLevel::Off`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// Reasoning disabled.
    None,
    /// Smallest effort.
    Minimal,
    /// Light effort.
    Low,
    /// Medium effort.
    Medium,
    /// Heavy effort.
    High,
    /// Heavier than high.
    XHigh,
    /// Heavier than xhigh.
    Max,
    /// The largest effort, above max.
    Ultra,
}

impl Effort {
    /// Every effort, lowest first.
    pub const ALL: [Self; 8] = [
        Self::None,
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
        Self::Ultra,
    ];

    /// Wire value, such as `"xhigh"`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
            Self::Ultra => "ultra",
        }
    }

    /// Parse a wire value produced by [`Self::as_str`].
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == s)
    }
}

/// A set of [`Effort`] values. Serializes as a list of wire values.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct Efforts(u8);

impl Efforts {
    /// The set holding `values`.
    #[must_use]
    pub const fn of(values: &[Effort]) -> Self {
        let mut bits = 0;
        let mut i = 0;
        while i < values.len() {
            bits |= 1 << values[i] as u8;
            i += 1;
        }
        Self(bits)
    }

    /// Whether `effort` is in the set.
    #[must_use]
    pub fn contains(self, effort: Effort) -> bool {
        self.0 & (1 << effort as u8) != 0
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// The efforts in the set, lowest first.
    pub fn iter(self) -> impl Iterator<Item = Effort> {
        Effort::ALL.into_iter().filter(move |e| self.contains(*e))
    }

    /// The effort that carries `level`, if the set has one.
    #[must_use]
    pub fn for_level(self, level: ThinkingLevel) -> Option<Effort> {
        let wanted = match level {
            ThinkingLevel::Off => Effort::None,
            ThinkingLevel::Minimal => Effort::Minimal,
            ThinkingLevel::Low => Effort::Low,
            ThinkingLevel::Medium => Effort::Medium,
            ThinkingLevel::High => Effort::High,
            ThinkingLevel::XHigh => Effort::XHigh,
            ThinkingLevel::Max => Effort::Max,
            ThinkingLevel::Ultra => Effort::Ultra,
        };
        self.contains(wanted).then_some(wanted)
    }
}

impl FromIterator<Effort> for Efforts {
    fn from_iter<I: IntoIterator<Item = Effort>>(iter: I) -> Self {
        Self(iter.into_iter().fold(0, |bits, e| bits | 1 << e as u8))
    }
}

impl Serialize for Efforts {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for Efforts {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Vec::<Effort>::deserialize(deserializer)?
            .into_iter()
            .collect())
    }
}

/// The thinking settings a model accepts.
///
/// Built from the model catalog (models.dev `reasoning` and
/// `reasoning_options`) or from a custom provider's config. Providers
/// read it to shape the request; frontends read [`Self::levels`] to
/// offer only levels the model takes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Reasoning {
    /// Nothing is known about the model. Explicit levels pass through
    /// unchanged and the automatic default sends nothing.
    #[default]
    Unknown,
    /// The model does not think.
    None,
    /// The model thinks and offers no setting.
    Fixed,
    /// Thinking is only switched on or off. On is shown as
    /// [`ThinkingLevel::High`].
    Toggle,
    /// The model takes a named effort.
    Effort {
        /// Efforts the model accepts.
        efforts: Efforts,
        /// Whether thinking can also be switched off outright.
        toggle: bool,
    },
    /// The model takes a thinking token budget.
    Budget {
        /// Smallest budget the model accepts.
        min: u32,
        /// Largest budget the model accepts, when bounded.
        max: Option<u32>,
        /// Whether thinking can also be switched off.
        toggle: bool,
    },
}

impl Reasoning {
    /// Levels the model accepts, lowest first. Empty when the model has
    /// no thinking setting.
    #[must_use]
    pub fn levels(self) -> Vec<ThinkingLevel> {
        let all = ThinkingLevel::ALL.into_iter();
        match self {
            Self::Unknown => all.collect(),
            Self::None | Self::Fixed => Vec::new(),
            Self::Toggle => vec![ThinkingLevel::Off, ThinkingLevel::High],
            Self::Effort { efforts, toggle } => all
                .filter(|l| (l.is_off() && toggle) || efforts.for_level(*l).is_some())
                .collect(),
            Self::Budget { toggle, .. } => all
                .filter(|l| (toggle || !l.is_off()) && *l != ThinkingLevel::Ultra)
                .collect(),
        }
    }

    /// The level a request sends when the user chose `requested`, or
    /// `None` to send nothing. An unset choice asks for
    /// [`ThinkingLevel::High`]. A level the model does not accept moves
    /// to the nearest one it does, the higher one on a tie, and only an
    /// explicit `off` (or a model whose only level is off) turns
    /// thinking off.
    #[must_use]
    pub fn resolve(self, requested: Option<ThinkingLevel>) -> Option<ThinkingLevel> {
        if self == Self::Unknown {
            return requested;
        }
        let levels = self.levels();
        let target = requested.unwrap_or(ThinkingLevel::High);
        if target.is_off() {
            return levels.first().copied();
        }
        let distance = |l: ThinkingLevel| (l as i8 - target as i8).unsigned_abs();
        levels
            .iter()
            .copied()
            .filter(|l| !l.is_off())
            .min_by_key(|l| (distance(*l), std::cmp::Reverse(*l)))
            .or_else(|| levels.first().copied())
    }

    /// The wire effort for `level` on an effort model. `None` for other
    /// kinds, and for [`ThinkingLevel::Off`] on a model without a
    /// `none` effort.
    #[must_use]
    pub fn effort(self, level: ThinkingLevel) -> Option<Effort> {
        match self {
            Self::Effort { efforts, .. } => efforts.for_level(level),
            _ => None,
        }
    }

    /// The thinking token budget for `level`, kept within the model's
    /// bounds on a budget model. `None` for [`ThinkingLevel::Off`].
    #[must_use]
    pub fn budget(self, level: ThinkingLevel) -> Option<u32> {
        let tokens = level.default_budget_tokens()?;
        Some(match self {
            Self::Budget { min, max, .. } => tokens.max(min).min(max.unwrap_or(u32::MAX)),
            _ => tokens,
        })
    }

    /// Whether the model can switch thinking off without an effort
    /// value: a toggle, or a budget model with a toggle.
    #[must_use]
    pub fn has_toggle(self) -> bool {
        match self {
            Self::Toggle => true,
            Self::Effort { toggle, .. } | Self::Budget { toggle, .. } => toggle,
            _ => false,
        }
    }
}

/// The assistant message field an OpenAI-compatible model reads its
/// own reasoning back from during a tool loop (models.dev
/// `interleaved.field`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningField {
    /// Reasoning text in `reasoning_content`.
    ReasoningContent,
    /// `OpenRouter` `reasoning_details` entries.
    ReasoningDetails,
}

impl ReasoningField {
    /// Wire name of the field.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReasoningContent => "reasoning_content",
            Self::ReasoningDetails => "reasoning_details",
        }
    }

    /// Parse a wire name produced by [`Self::as_str`].
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        [Self::ReasoningContent, Self::ReasoningDetails]
            .into_iter()
            .find(|f| f.as_str() == s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_serializes_as_snake_case() {
        let v = serde_json::to_value(ThinkingLevel::XHigh).unwrap();
        assert_eq!(v, serde_json::json!("xhigh"));
        let back: ThinkingLevel = serde_json::from_value(v).unwrap();
        assert_eq!(back, ThinkingLevel::XHigh);
    }

    #[test]
    fn level_parse_roundtrips_each_variant() {
        for lv in ThinkingLevel::ALL {
            assert_eq!(ThinkingLevel::parse(lv.as_str()), Some(lv));
            let wire = serde_json::to_value(lv).unwrap();
            assert_eq!(wire, serde_json::json!(lv.as_str()));
        }
        assert_eq!(ThinkingLevel::parse("nope"), None);
    }

    #[test]
    fn openai_reasoning_effort_caps_at_high() {
        assert_eq!(ThinkingLevel::Off.openai_reasoning_effort(), None);
        assert_eq!(
            ThinkingLevel::Minimal.openai_reasoning_effort(),
            Some("minimal")
        );
        assert_eq!(ThinkingLevel::High.openai_reasoning_effort(), Some("high"));
        assert_eq!(ThinkingLevel::XHigh.openai_reasoning_effort(), Some("high"));
        assert_eq!(ThinkingLevel::Max.openai_reasoning_effort(), Some("high"));
    }

    #[test]
    fn default_budget_tokens_increases_with_level() {
        assert_eq!(ThinkingLevel::Off.default_budget_tokens(), None);
        assert!(
            ThinkingLevel::Minimal.default_budget_tokens()
                < ThinkingLevel::Low.default_budget_tokens()
        );
        assert!(
            ThinkingLevel::Low.default_budget_tokens()
                < ThinkingLevel::Medium.default_budget_tokens()
        );
        assert!(
            ThinkingLevel::Medium.default_budget_tokens()
                < ThinkingLevel::High.default_budget_tokens()
        );
        assert!(
            ThinkingLevel::High.default_budget_tokens()
                < ThinkingLevel::XHigh.default_budget_tokens()
        );
        assert!(
            ThinkingLevel::XHigh.default_budget_tokens()
                < ThinkingLevel::Max.default_budget_tokens()
        );
    }

    use ThinkingLevel::{High, Low, Max, Medium, Minimal, Off, Ultra, XHigh};

    fn effort(values: &[Effort], toggle: bool) -> Reasoning {
        Reasoning::Effort {
            efforts: Efforts::of(values),
            toggle,
        }
    }

    #[test]
    fn auto_is_high_when_the_model_allows_it() {
        let r = effort(&[Effort::Low, Effort::Medium, Effort::High], false);
        assert_eq!(r.resolve(None), Some(High));
        let budget = Reasoning::Budget {
            min: 1024,
            max: None,
            toggle: true,
        };
        assert_eq!(budget.resolve(None), Some(High));
        assert_eq!(Reasoning::Toggle.resolve(None), Some(High));
    }

    #[test]
    fn auto_moves_to_the_nearest_level() {
        assert_eq!(effort(&[Effort::Medium], false).resolve(None), Some(Medium));
        assert_eq!(
            effort(&[Effort::Low, Effort::Max], false).resolve(None),
            Some(Max)
        );
        let tie = effort(&[Effort::Minimal, Effort::Medium, Effort::XHigh], false);
        assert_eq!(tie.resolve(None), Some(XHigh));
    }

    #[test]
    fn explicit_levels_are_clamped_and_off_needs_asking() {
        let r = effort(&[Effort::None, Effort::High], false);
        assert_eq!(r.resolve(Some(Low)), Some(High));
        assert_eq!(r.resolve(Some(Off)), Some(Off));
        let no_off = effort(&[Effort::Low, Effort::High], false);
        assert_eq!(no_off.resolve(Some(Off)), Some(Low));
        assert_eq!(no_off.resolve(Some(Minimal)), Some(Low));
    }

    #[test]
    fn models_without_a_setting_send_nothing() {
        assert_eq!(Reasoning::None.resolve(Some(High)), None);
        assert_eq!(Reasoning::Fixed.resolve(None), None);
        assert!(Reasoning::None.levels().is_empty());
    }

    #[test]
    fn unknown_models_pass_explicit_levels_through() {
        assert_eq!(Reasoning::Unknown.resolve(None), None);
        assert_eq!(Reasoning::Unknown.resolve(Some(Minimal)), Some(Minimal));
        assert_eq!(Reasoning::Unknown.levels(), ThinkingLevel::ALL.to_vec());
    }

    #[test]
    fn levels_follow_efforts_and_toggle() {
        let r = effort(&[Effort::Low, Effort::High, Effort::Max], true);
        assert_eq!(r.levels(), vec![Off, Low, High, Max]);
        assert_eq!(r.effort(Max), Some(Effort::Max));
        assert_eq!(r.effort(XHigh), None);
        assert_eq!(r.resolve(Some(XHigh)), Some(Max));
        let both = effort(&[Effort::XHigh, Effort::Max], false);
        assert_eq!(both.levels(), vec![XHigh, Max]);
        assert_eq!(both.effort(XHigh), Some(Effort::XHigh));
        assert_eq!(both.effort(Max), Some(Effort::Max));
        assert_eq!(Reasoning::Toggle.levels(), vec![Off, High]);
    }

    #[test]
    fn budget_stays_within_bounds() {
        let r = Reasoning::Budget {
            min: 2048,
            max: Some(24_576),
            toggle: false,
        };
        assert_eq!(r.budget(Minimal), Some(2048));
        assert_eq!(r.budget(XHigh), Some(24_576));
        assert_eq!(r.budget(Off), None);
        assert!(!r.levels().contains(&Off));
    }

    /// `ultra` is a named effort only: an effort model that lists it
    /// offers it, and a budget model tops out at `max`.
    #[test]
    fn ultra_is_offered_only_by_effort_models() {
        let codex = effort(&[Effort::Low, Effort::Max, Effort::Ultra], false);
        assert_eq!(codex.levels(), vec![Low, Max, Ultra]);
        assert_eq!(codex.effort(Ultra), Some(Effort::Ultra));
        assert_eq!(codex.resolve(Some(Ultra)), Some(Ultra));
        assert_eq!(
            effort(&[Effort::Max], false).resolve(Some(Ultra)),
            Some(Max)
        );

        let budget = Reasoning::Budget {
            min: 1024,
            max: None,
            toggle: true,
        };
        assert_eq!(budget.levels().last(), Some(&Max));
        assert_eq!(budget.resolve(Some(Ultra)), Some(Max));

        assert_eq!(ThinkingLevel::parse("ultra"), Some(Ultra));
        assert_eq!(Effort::parse("ultra"), Some(Effort::Ultra));
    }

    #[test]
    fn next_in_wraps_within_the_given_levels() {
        let levels = [Low, High, XHigh];
        assert_eq!(ThinkingLevel::next_in(&levels, Some(Low)), Some(High));
        assert_eq!(ThinkingLevel::next_in(&levels, Some(XHigh)), Some(Low));
        assert_eq!(ThinkingLevel::next_in(&levels, Some(Medium)), Some(High));
        assert_eq!(ThinkingLevel::next_in(&levels, None), Some(Low));
        assert_eq!(ThinkingLevel::next_in(&[], Some(Low)), None);
    }

    #[test]
    fn reasoning_roundtrips_through_json() {
        let r = effort(&[Effort::None, Effort::XHigh], true);
        let v = serde_json::to_value(r).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"kind": "effort", "efforts": ["none", "xhigh"], "toggle": true})
        );
        assert_eq!(serde_json::from_value::<Reasoning>(v).unwrap(), r);
    }
}
