//! The window's look, after ChatGPT's: neutral grays, rounded controls and
//! one blue accent, light or dark as the system is.

use std::rc::Rc;

use gpui_kit::{
    Anchor, App,
    component::{Theme, ThemeSet},
};

/// The light and dark themes, in gpui-component's theme format. Colors left
/// out keep gpui-component's defaults.
const THEMES: &str = include_str!("theme.json");

/// Puts Peerflix's themes in place of the defaults, which the window picks
/// between as it follows the system's appearance.
pub fn init(cx: &mut App) {
    let themes: ThemeSet = serde_json::from_str(THEMES).expect("theme.json parses");
    Theme::update(cx, |theme| {
        for config in themes.themes {
            if config.mode.is_dark() {
                theme.dark_theme = Rc::new(config);
            } else {
                theme.light_theme = Rc::new(config);
            }
        }
        // The search box keeps the focus, and a ring around it all the time
        // is more than ChatGPT's composer shows.
        theme.focus_ring = false;
        // Out of the way of the search bar and the results' header.
        theme.notification.placement = Anchor::BottomCenter;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_light_and_dark_themes() {
        let themes: ThemeSet = serde_json::from_str(THEMES).unwrap();
        let dark: Vec<_> = themes.themes.iter().map(|t| t.mode.is_dark()).collect();
        assert_eq!(dark, [false, true]);
    }

    /// A misspelt color would be ignored rather than fail to parse.
    #[test]
    fn every_color_is_known() {
        let raw: serde_json::Value = serde_json::from_str(THEMES).unwrap();
        let themes: ThemeSet = serde_json::from_str(THEMES).unwrap();
        for (raw, parsed) in raw["themes"].as_array().unwrap().iter().zip(&themes.themes) {
            let parsed = serde_json::to_value(&parsed.colors).unwrap();
            for (key, color) in raw["colors"].as_object().unwrap() {
                assert_eq!(&parsed[key], color, "{} {key}", raw["name"]);
            }
        }
    }
}
