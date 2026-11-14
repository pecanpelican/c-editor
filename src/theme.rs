use egui::Color32;

use crate::highlight::Tok;

#[derive(Clone)]
pub struct Theme {
    pub bg: Color32,
    pub panel_bg: Color32,
    pub gutter_fg: Color32,
    pub gutter_current_fg: Color32,
    pub current_line_bg: Color32,
    pub text: Color32,
    pub keyword: Color32,
    pub ty: Color32,
    pub function: Color32,
    pub number: Color32,
    pub string: Color32,
    pub comment: Color32,
    pub preproc: Color32,
    pub constant: Color32,
    pub punct: Color32,
    pub variable: Color32,
    pub target: Color32,
    pub match_bg: Color32,
    pub current_match_bg: Color32,
    pub error: Color32,
    pub warning: Color32,
    pub italic_comments: bool,
    pub dark: bool,
    /// Selection background in text fields and the editor.
    pub selection: Color32,
}

const fn hex(rgb: u32) -> Color32 {
    Color32::from_rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

impl Theme {
    pub fn dark() -> Self {
        Self {
            bg: hex(0x1b1d23),
            panel_bg: hex(0x21242b),
            gutter_fg: hex(0x5a606c),
            gutter_current_fg: hex(0xc6cad3),
            current_line_bg: hex(0x23262e),
            text: hex(0xd4d7de),
            keyword: hex(0xe0956a),
            ty: hex(0x7cc4d4),
            function: hex(0x6fb0f2),
            number: hex(0xc79bf2),
            string: hex(0x9ccc7a),
            comment: hex(0x6b7280),
            preproc: hex(0xd6c36b),
            constant: hex(0xe8a3c4),
            punct: hex(0xa5abb6),
            variable: hex(0x7cc4d4),
            target: hex(0x6fb0f2),
            match_bg: Color32::from_rgba_unmultiplied(214, 195, 107, 50),
            current_match_bg: Color32::from_rgba_unmultiplied(224, 149, 106, 120),
            error: hex(0xf0696a),
            warning: hex(0xe5b350),
            italic_comments: true,
            dark: true,
            selection: hex(0x2f4a6e),
        }
    }

    pub fn light() -> Self {
        Self {
            bg: hex(0xfbfbfc),
            panel_bg: hex(0xf0f1f4),
            gutter_fg: hex(0xa3a8b3),
            gutter_current_fg: hex(0x3b3f47),
            current_line_bg: hex(0xf1f3f7),
            text: hex(0x24272e),
            keyword: hex(0xa8460b),
            ty: hex(0x0b6e80),
            function: hex(0x1f5fbf),
            number: hex(0x7a3bc4),
            string: hex(0x2f7a1f),
            comment: hex(0x8a8f99),
            preproc: hex(0x8a6d00),
            constant: hex(0xb0306c),
            punct: hex(0x5c616b),
            variable: hex(0x0b6e80),
            target: hex(0x1f5fbf),
            match_bg: Color32::from_rgba_unmultiplied(230, 200, 60, 90),
            current_match_bg: Color32::from_rgba_unmultiplied(240, 140, 60, 140),
            error: hex(0xd0313a),
            warning: hex(0xb57800),
            italic_comments: true,
            dark: false,
            selection: hex(0xc5d9f5),
        }
    }

    /// Deep blue-black with saturated accents.
    pub fn midnight() -> Self {
        Self {
            bg: hex(0x10131a),
            panel_bg: hex(0x151922),
            gutter_fg: hex(0x4a5263),
            gutter_current_fg: hex(0xb8c0d0),
            current_line_bg: hex(0x181c27),
            text: hex(0xcdd3de),
            keyword: hex(0xc79bf2),
            ty: hex(0x5fd3c4),
            function: hex(0x82aaff),
            number: hex(0xf78c6c),
            string: hex(0xc3e88d),
            comment: hex(0x5f6b7d),
            preproc: hex(0xffcb6b),
            constant: hex(0xff9cac),
            punct: hex(0x8f9bb3),
            variable: hex(0x5fd3c4),
            target: hex(0x82aaff),
            match_bg: Color32::from_rgba_unmultiplied(255, 203, 107, 45),
            current_match_bg: Color32::from_rgba_unmultiplied(247, 140, 108, 120),
            error: hex(0xff5370),
            warning: hex(0xffcb6b),
            italic_comments: true,
            dark: true,
            selection: hex(0x263a5e),
        }
    }

    /// Black background, bright text, no italics: easiest to read.
    pub fn high_contrast() -> Self {
        Self {
            bg: hex(0x000000),
            panel_bg: hex(0x0c0c0c),
            gutter_fg: hex(0x8a8a8a),
            gutter_current_fg: hex(0xffffff),
            current_line_bg: hex(0x1c1c1c),
            text: hex(0xffffff),
            keyword: hex(0xff9e57),
            ty: hex(0x4fe0ff),
            function: hex(0x7cb7ff),
            number: hex(0xe0a0ff),
            string: hex(0x9cff6e),
            comment: hex(0xa8b0bd),
            preproc: hex(0xffe066),
            constant: hex(0xff8fc8),
            punct: hex(0xd8d8d8),
            variable: hex(0x4fe0ff),
            target: hex(0x7cb7ff),
            match_bg: Color32::from_rgba_unmultiplied(255, 224, 102, 70),
            current_match_bg: Color32::from_rgba_unmultiplied(255, 158, 87, 150),
            error: hex(0xff4d4d),
            warning: hex(0xffd000),
            italic_comments: false,
            dark: true,
            selection: hex(0x1f4f8f),
        }
    }

    /// Warm, low-glare light theme.
    pub fn sepia() -> Self {
        Self {
            bg: hex(0xf6f0e3),
            panel_bg: hex(0xebe3d1),
            gutter_fg: hex(0xb0a48c),
            gutter_current_fg: hex(0x4a4030),
            current_line_bg: hex(0xefe7d5),
            text: hex(0x3b3326),
            keyword: hex(0x9c3d10),
            ty: hex(0x1d6b70),
            function: hex(0x2a55a0),
            number: hex(0x7b3fa0),
            string: hex(0x4f7a12),
            comment: hex(0x968a74),
            preproc: hex(0x8a6400),
            constant: hex(0xa8325e),
            punct: hex(0x6e6352),
            variable: hex(0x1d6b70),
            target: hex(0x2a55a0),
            match_bg: Color32::from_rgba_unmultiplied(220, 180, 60, 90),
            current_match_bg: Color32::from_rgba_unmultiplied(230, 130, 50, 140),
            error: hex(0xc0262d),
            warning: hex(0xa66b00),
            italic_comments: true,
            dark: false,
            selection: hex(0xe0d0a8),
        }
    }

    /// Theme names accepted in settings.json, with display names.
    pub const NAMES: &'static [(&'static str, &'static str)] = &[
        ("system", "Match System"),
        ("dark", "Dark"),
        ("light", "Light"),
        ("midnight", "Midnight"),
        ("high-contrast", "High Contrast"),
        ("sepia", "Sepia"),
    ];

    /// The theme for a settings name. "system" follows the OS (dark when unknown).
    pub fn by_name(name: &str, system_dark: Option<bool>) -> Self {
        match name.trim().to_ascii_lowercase().replace(['_', ' '], "-").as_str() {
            "light" => Self::light(),
            "midnight" => Self::midnight(),
            "high-contrast" | "contrast" => Self::high_contrast(),
            "sepia" => Self::sepia(),
            "system" | "auto" => {
                if system_dark == Some(false) {
                    Self::light()
                } else {
                    Self::dark()
                }
            }
            _ => Self::dark(),
        }
    }

    pub fn token_color(&self, tok: Tok) -> Color32 {
        match tok {
            Tok::Normal => self.text,
            Tok::Keyword => self.keyword,
            Tok::Type => self.ty,
            Tok::Function => self.function,
            Tok::Number => self.number,
            Tok::Str => self.string,
            Tok::Comment => self.comment,
            Tok::Preproc => self.preproc,
            Tok::Constant => self.constant,
            Tok::Punct => self.punct,
            Tok::Variable => self.variable,
            Tok::Target => self.target,
        }
    }

    pub fn apply(&self, ctx: &egui::Context) {
        let mut visuals = if self.dark { egui::Visuals::dark() } else { egui::Visuals::light() };
        visuals.panel_fill = self.panel_bg;
        visuals.window_fill = self.panel_bg;
        visuals.extreme_bg_color = self.bg;
        visuals.faint_bg_color = self.current_line_bg;
        visuals.selection.bg_fill = self.selection;
        visuals.window_stroke.color = self.gutter_fg.gamma_multiply(0.5);
        visuals.error_fg_color = self.error;
        visuals.warn_fg_color = self.warning;
        visuals.selection.stroke.color = self.text;
        visuals.hyperlink_color = self.function;
        if self.dark {
            ctx.set_visuals_of(egui::Theme::Dark, visuals);
            ctx.set_theme(egui::ThemePreference::Dark);
        } else {
            ctx.set_visuals_of(egui::Theme::Light, visuals);
            ctx.set_theme(egui::ThemePreference::Light);
        }
    }
}
