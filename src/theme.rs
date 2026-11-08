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
        visuals.selection.bg_fill = if self.dark {
            Color32::from_rgb(0x2f, 0x4a, 0x6e)
        } else {
            Color32::from_rgb(0xc5, 0xd9, 0xf5)
        };
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
