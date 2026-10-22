use std::cmp::Ordering;
use std::time::{SystemTime, UNIX_EPOCH};

use super::{Context, Module, ModuleConfig};
use crate::configs::claude_context::ClaudeDisplayConfig;
use crate::configs::claude_usage::ClaudeUsageConfig;
use crate::context::RateLimitWindow;
use crate::formatter::StringFormatter;
use crate::utils::render_time_short;

/// Usage of a window, guarded against a payload reporting a percentage outside 0-100
fn used(window: &RateLimitWindow) -> f32 {
    window.used_percentage.clamp(0.0, 100.0)
}

/// A window paired with the `display` entry that styles it: the highest threshold at or below
/// its own usage. `None` when the window was not reported, no threshold matches, or the match
/// is `hidden`, so each window can be shown or hidden independently.
fn shown<'a>(
    display: &'a [ClaudeDisplayConfig<'a>],
    window: Option<&'a RateLimitWindow>,
) -> Option<(&'a RateLimitWindow, &'a ClaudeDisplayConfig<'a>)> {
    let window = window?;
    let style = display
        .iter()
        .filter(|s| used(window) >= s.threshold)
        .max_by(|a, b| {
            a.threshold
                .partial_cmp(&b.threshold)
                .unwrap_or(Ordering::Equal)
        })
        .filter(|s| !s.hidden)?;
    Some((window, style))
}

pub fn module<'a>(context: &'a Context) -> Option<Module<'a>> {
    let mut module = context.new_module("claude_usage");
    let config = ClaudeUsageConfig::try_load(module.config);

    if config.disabled {
        return None;
    }

    // Only reported for Claude.ai subscriptions, and only after the first API response
    let rate_limits = context.claude_code_data.as_ref()?.rate_limits.as_ref()?;
    let five_hour = shown(&config.display, rate_limits.five_hour.as_ref());
    let seven_day = shown(&config.display, rate_limits.seven_day.as_ref());

    // `$style` follows the busiest window being shown. With neither shown there is nothing to render.
    let display_style = [five_hour, seven_day]
        .into_iter()
        .flatten()
        .max_by(|(a, _), (b, _)| used(a).partial_cmp(&used(b)).unwrap_or(Ordering::Equal))
        .map(|(_, style)| style)?;

    // A hidden window is treated like one that was never reported, which drops its group
    let five_hour_window = five_hour.map(|(window, _)| window);
    let seven_day_window = seven_day.map(|(window, _)| window);

    let now_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let percentage = |window: Option<&RateLimitWindow>| {
        window.map(|w| Ok(format!("{}%", used(w).round() as u8)))
    };
    // Unset when the reset time is unknown or already past
    let reset = |window: Option<&RateLimitWindow>| {
        let seconds = window?.resets_at.checked_sub(now_secs).filter(|s| *s > 0)?;
        Some(Ok(render_time_short(u128::from(seconds) * 1000)))
    };

    let parsed = StringFormatter::new(config.format).and_then(|formatter| {
        formatter
            .map_meta(|variable, _| match variable {
                "symbol" => Some(config.symbol),
                _ => None,
            })
            .map_style(|variable| match variable {
                "style" => Some(Ok(display_style.style)),
                "five_hour_style" => five_hour.map(|(_, style)| Ok(style.style)),
                "seven_day_style" => seven_day.map(|(_, style)| Ok(style.style)),
                _ => None,
            })
            .map(|variable| match variable {
                "five_hour_percentage" => percentage(five_hour_window),
                "five_hour_reset" => reset(five_hour_window),
                "seven_day_percentage" => percentage(seven_day_window),
                "seven_day_reset" => reset(seven_day_window),
                _ => None,
            })
            .parse(None, Some(context))
    });

    module.set_segments(match parsed {
        Ok(segments) => segments,
        Err(error) => {
            log::warn!("Error in module `claude_usage`: {error}");
            return None;
        }
    });

    Some(module)
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::context::{ClaudeCodeData, RateLimitWindow, RateLimits};
    use crate::test::ModuleRenderer;
    use nu_ansi_term::{AnsiStrings, Color};

    fn make_data(
        five_hour: Option<RateLimitWindow>,
        seven_day: Option<RateLimitWindow>,
    ) -> ClaudeCodeData {
        ClaudeCodeData {
            rate_limits: Some(RateLimits {
                five_hour,
                seven_day,
            }),
            ..Default::default()
        }
    }

    fn window(used_percentage: f32) -> Option<RateLimitWindow> {
        Some(RateLimitWindow {
            used_percentage,
            resets_at: 0,
        })
    }

    fn render(data: ClaudeCodeData) -> Option<String> {
        ModuleRenderer::new("claude_usage")
            .config(toml::toml! {
                [claude_usage]
                format = "[$symbol]($style)([ 5h $five_hour_percentage]($five_hour_style))([ 7d $seven_day_percentage]($seven_day_style)) "
            })
            .claude_code_data(data)
            .collect()
    }

    #[test]
    fn test_without_data() {
        assert_eq!(ModuleRenderer::new("claude_usage").collect(), None);
        assert_eq!(render(ClaudeCodeData::default()), None);
        assert_eq!(render(make_data(None, None)), None);
    }

    #[test]
    fn test_disabled() {
        let actual = ModuleRenderer::new("claude_usage")
            .config(toml::toml! {
                [claude_usage]
                disabled = true
            })
            .claude_code_data(make_data(window(95.0), window(95.0)))
            .collect();
        assert_eq!(actual, None);
    }

    #[test]
    fn test_hidden_below_threshold() {
        assert_eq!(render(make_data(window(69.9), window(30.0))), None);
    }

    #[test]
    fn test_each_window_styled_by_own_usage() {
        // Thresholds are inclusive: exactly 90 is red and exactly 70 is yellow
        assert_eq!(
            render(make_data(window(90.0), window(70.0))),
            Some(format!(
                "{} ",
                AnsiStrings(&[
                    Color::Red.bold().paint("⏳ 5h 90%"),
                    Color::Yellow.bold().paint(" 7d 70%"),
                ])
            ))
        );
    }

    #[test]
    fn test_hidden_window_drops_its_group() {
        assert_eq!(
            render(make_data(window(20.0), window(94.5))),
            Some(format!("{} ", Color::Red.bold().paint("⏳ 7d 95%")))
        );
    }

    #[test]
    fn test_default_format_with_reset() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        // 30s of slack before `4h12m` would tick down to `4h11m`
        let data = make_data(
            Some(RateLimitWindow {
                used_percentage: 75.0,
                resets_at: now + 4 * 3600 + 12 * 60 + 30,
            }),
            // An unknown reset time drops just the reset group
            window(91.0),
        );
        let actual = ModuleRenderer::new("claude_usage")
            .claude_code_data(data)
            .collect();
        assert_eq!(
            actual,
            Some(format!(
                "{} ",
                AnsiStrings(&[
                    Color::Red.bold().paint("⏳"),
                    Color::Yellow.bold().paint(" 5h 75% (4h12m)"),
                    Color::Red.bold().paint(" 7d 91%"),
                ])
            ))
        );
    }

    #[test]
    fn test_missing_window_drops_its_group() {
        assert_eq!(
            render(make_data(window(75.0), None)),
            Some(format!("{} ", Color::Yellow.bold().paint("⏳ 5h 75%")))
        );
    }
}
