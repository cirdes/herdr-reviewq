use crate::tui::keys::confirm_text;
use crate::tui::model::{Section, UiState, View};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

pub const MIN_W: u16 = 60;
pub const MIN_H: u16 = 12;
const FOOTER: &str = "↑↓ mover · enter abrir · o GitHub · l log · s sync · R tentar de novo · a adotar · r liberar · q sair";
const MAX_CONFIRM_LINES: u16 = 4;
const BOTTOM_H: u16 = 5;

fn section_title(s: Section) -> &'static str {
    match s {
        Section::Pending => "",
        Section::NoSync => "Sem sync",
        Section::Leaving => "Saindo",
        Section::Adopted => "Adotados",
    }
}

/// Número de linhas que `text` ocupa com quebra gulosa por palavra em `width` colunas.
fn wrapped_lines(text: &str, width: u16) -> u16 {
    let width = width.max(1) as usize;
    let mut lines = 1usize;
    let mut cur = 0usize;
    for word in text.split_whitespace() {
        let mut w = word.chars().count();
        if cur > 0 && cur + 1 + w <= width {
            cur += 1 + w;
            continue;
        }
        if cur > 0 {
            lines += 1;
        }
        while w > width {
            lines += 1;
            w -= width;
        }
        cur = w;
    }
    lines.min(u16::MAX as usize) as u16
}

pub fn draw(f: &mut Frame, view: &View, ui: &UiState) {
    let area = f.area();
    if area.width < MIN_W || area.height < MIN_H {
        f.render_widget(Paragraph::new("aumente o pane para ver o reviewq"), area);
        return;
    }

    // A confirmação tem prioridade sobre qualquer mensagem e nunca é cortada.
    let confirm = ui.confirm.as_ref().map(|(kind, key)| confirm_text(*kind, key));
    let footer_h = match &confirm {
        Some(t) => wrapped_lines(t, area.width.saturating_sub(1)).min(MAX_CONFIRM_LINES),
        None => 1,
    };
    let fixed = 1 + 3 + 3 + footer_h;
    let bottom_h = BOTTOM_H.min(area.height.saturating_sub(fixed)).max(1);
    let [head, counters, list, bottom, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(bottom_h),
        Constraint::Length(footer_h),
    ])
    .areas(area);

    let sync_style = if view.header.stale || !view.header.errors.is_empty() { Style::new().fg(Color::Red) } else { Style::new() };
    f.render_widget(
        Paragraph::new(Line::from(vec![Span::raw(" reviewq   "), Span::styled(view.header.sync.clone(), sync_style)])),
        head,
    );

    let [c1, c2, c3] = Layout::horizontal([Constraint::Length(14), Constraint::Length(16), Constraint::Min(10)]).areas(counters);
    f.render_widget(Paragraph::new(format!(" {}", view.pending)).block(Block::default().borders(Borders::ALL).title("Pendentes")), c1);
    f.render_widget(Paragraph::new(format!(" {}", view.today)).block(Block::default().borders(Borders::ALL).title("Feitas hoje")), c2);
    f.render_widget(Paragraph::new(view.today_detail.clone()), Rect { y: c3.y + 1, height: 1, ..c3 });

    draw_list(f, list, view, ui);
    draw_bottom(f, bottom, view);

    match confirm {
        Some(text) => f.render_widget(
            Paragraph::new(format!(" {text}"))
                .wrap(Wrap { trim: true })
                .style(Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            footer,
        ),
        None => {
            let text = ui.message.clone().unwrap_or_else(|| FOOTER.to_string());
            f.render_widget(Paragraph::new(Span::styled(format!(" {text}"), Style::new().fg(Color::DarkGray))), footer);
        }
    }
}

/// Linha do pedido reservada na última linha; erros e alertas usam o restante, com "+N".
fn draw_bottom(f: &mut Frame, area: Rect, view: &View) {
    let [info, request] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(area);
    let mut lines: Vec<Line> = view.header.errors.iter().map(|e| Line::styled(format!(" ! {e}"), Style::new().fg(Color::Red))).collect();
    if !view.alerts.is_empty() {
        lines.push(Line::styled(" Alertas", Style::new().add_modifier(Modifier::BOLD)));
        lines.extend(view.alerts.iter().map(|a| Line::raw(format!("  {a}"))));
    }
    if let Some(e) = &view.empty_reason {
        lines.push(Line::raw(format!(" {e}")));
    }
    let cap = info.height as usize;
    if lines.len() > cap {
        if cap > 1 {
            let hidden = lines.len() - (cap - 1);
            lines.truncate(cap - 1);
            lines.push(Line::styled(format!("  +{hidden}"), Style::new().fg(Color::DarkGray)));
        } else {
            lines.truncate(cap);
        }
    }
    f.render_widget(Paragraph::new(lines), info);
    if let Some(r) = &view.request_line {
        f.render_widget(Paragraph::new(format!(" {r}")), request);
    }
}

fn clip(s: &str, w: usize) -> String {
    s.chars().take(w).collect()
}

fn draw_list(f: &mut Frame, area: Rect, view: &View, ui: &UiState) {
    // Colunas: marcador(2) rótulo(12) ícone(1) estado, depois autor e título com o que sobra.
    const PREFIX: usize = 2 + 12 + 1 + 1 + 1;
    let rest = (area.width as usize).saturating_sub(PREFIX);
    let max_status = view.rows.iter().map(|r| r.status.chars().count()).max().unwrap_or(6).max(6);
    let status_w = max_status.min(rest * 3 / 5).min(30);
    let author_w = 16.min(rest.saturating_sub(status_w + 2) / 3);
    let title_w = rest.saturating_sub(status_w + author_w + 2);
    let selected = ui.selected_row(view).map(|r| r.key.clone());
    let head = format!("{:<w$} {:<sw$} {:<aw$} {}", "  PR", "Estado", "Autor", "Título", w = PREFIX - 1, sw = status_w, aw = author_w);
    let mut lines: Vec<Line> = vec![Line::styled(clip(&head, area.width as usize), Style::new().add_modifier(Modifier::DIM))];
    let mut current: Option<Section> = None;
    let mut selected_line = 0usize;
    for row in &view.rows {
        if current != Some(row.section) {
            current = Some(row.section);
            let title = section_title(row.section);
            if !title.is_empty() {
                lines.push(Line::styled(format!(" {title}"), Style::new().add_modifier(Modifier::BOLD)));
            }
        }
        let is_sel = selected.as_ref() == Some(&row.key);
        if is_sel {
            selected_line = lines.len();
        }
        let text = format!(
            "{} {:<12} {} {:<sw$} {:<aw$} {}",
            if is_sel { "▸" } else { " " },
            clip(&row.label, 12),
            row.icon,
            clip(&row.status, status_w),
            clip(&row.author, author_w),
            clip(&row.title, title_w),
            sw = status_w,
            aw = author_w
        );
        let style = if is_sel { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
        lines.push(Line::styled(text, style));
    }
    let height = area.height as usize;
    let offset = selected_line.saturating_sub(height.saturating_sub(1));
    f.render_widget(Paragraph::new(lines).scroll((offset.min(u16::MAX as usize) as u16, 0)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::requests::RequestKind;
    use crate::state::{Ownership, PrKey, PrRecord, RepoStatus, State};
    use crate::tui::model::{UiFacts, View, ViewConfig};
    use chrono::Utc;
    use ratatui::{backend::TestBackend, Terminal};

    fn screen(w: u16, h: u16, view: &View, ui: &UiState) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, view, ui)).unwrap();
        let buf = t.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n").collect()
    }

    fn cfg() -> ViewConfig {
        ViewConfig { repos: vec!["o/r".into()], poll_interval_secs: 60, remove_grace: chrono::Duration::minutes(15) }
    }

    fn base_state() -> State {
        let mut s = State::default();
        s.repos.insert("o/r".into(), RepoStatus { ok: true, last_sync: Some(Utc::now()), ..Default::default() });
        s
    }

    fn build(s: &State, ui: &UiState) -> View {
        View::build(Some(s), ui, &cfg(), &UiFacts::default(), Utc::now())
    }

    fn sample() -> View {
        let mut s = base_state();
        s.insert(PrRecord::fixture("o/r", 15672, "x", "s"));
        let mut a = PrRecord::fixture("o/r", 15726, "y", "s");
        a.ownership = Ownership::Adopted { reason: "trocou para stack/x".into(), at: Utc::now() };
        s.insert(a);
        s.push_alert("branch feat/x preservada".into());
        build(&s, &UiState::default())
    }

    #[test]
    fn draws_counters_sections_and_footer() {
        let out = screen(110, 24, &sample(), &UiState::default());
        for needle in ["Pendentes", "Feitas hoje", "#15672", "Adotados", "#15726", "trocou para stack/x", "branch feat/x preservada", "enter abrir"] {
            assert!(out.contains(needle), "faltou {needle}:\n{out}");
        }
    }

    #[test]
    fn confirmation_replaces_footer_and_is_complete() {
        let key = PrKey::new("o/r", 15726);
        let full = confirm_text(RequestKind::Release, &key);
        let ui = UiState {
            confirm: Some((RequestKind::Release, key)),
            message: Some("mensagem comum que não deve aparecer".into()),
            ..Default::default()
        };
        for w in [60u16, 110] {
            let out = screen(w, 24, &sample(), &ui);
            assert!(out.contains("liberar o/r#15726?"), "{w}:\n{out}");
            assert!(out.contains("(y/n)"), "{w}:\n{out}");
            assert!(!out.contains("mensagem comum"), "{w}:\n{out}");
            assert!(!out.contains("enter abrir"), "{w}:\n{out}");
            // todas as palavras do texto estão na tela
            for word in full.split_whitespace() {
                assert!(out.contains(word), "{w} faltou {word}:\n{out}");
            }
        }
    }

    #[test]
    fn tiny_terminal_asks_for_space() {
        assert!(screen(30, 6, &sample(), &UiState::default()).contains("aumente"));
    }

    #[test]
    fn request_line_survives_error_and_alerts() {
        let mut s = base_state();
        s.insert(PrRecord::fixture("o/r", 1, "x", "s"));
        for i in 0..3 {
            s.push_alert(format!("alerta numero {i}"));
        }
        let mut view = build(&s, &UiState::default());
        view.header.errors = vec!["falha na busca do GitHub".into()];
        view.request_line = Some("pedido sync enviado".into());
        let out = screen(110, 24, &view, &UiState::default());
        assert!(out.contains("pedido sync enviado"), "{out}");
        assert!(out.contains("falha na busca"), "{out}");
        assert!(out.contains("+2"), "esperava marcador +2:\n{out}");
    }

    #[test]
    fn long_list_keeps_selection_visible() {
        let mut s = base_state();
        for n in 1..=60u64 {
            s.insert(PrRecord::fixture("o/r", 1000 + n, "x", "s"));
        }
        let ui0 = UiState::default();
        let view = build(&s, &ui0);
        let last = view.rows.last().unwrap().key.clone();
        let label = view.rows.last().unwrap().label.clone();
        let ui = UiState { selected: Some(last), ..Default::default() };
        let out = screen(110, 20, &view, &ui);
        assert!(out.contains(&format!("▸ {label}")), "{out}");
    }

    fn failed_view() -> View {
        let mut s = base_state();
        let mut r = PrRecord::fixture("o/r", 15672, "x", "s");
        r.phase = crate::state::Phase::Failed { step: "setup".into(), reason: "e".into() };
        s.insert(r);
        build(&s, &UiState::default())
    }

    #[test]
    fn status_visible_at_min_width() {
        let view = failed_view();
        let icon = view.rows[0].icon;
        let out = screen(60, 14, &view, &UiState::default());
        assert!(out.contains("falhou"), "{out}");
        assert!(out.contains(icon), "{out}");
    }

    #[test]
    fn min_size_with_confirmation() {
        let key = PrKey::new("o/r", 15726);
        let ui = UiState { confirm: Some((RequestKind::Release, key)), ..Default::default() };
        let out = screen(60, 12, &sample(), &ui);
        assert!(out.contains("(y/n)") && out.contains("liberar o/r#15726?"), "{out}");
    }

    #[test]
    fn min_size_with_error_alerts_request() {
        let mut view = sample();
        view.header.errors = vec!["falha na busca".into()];
        view.alerts = vec!["a1".into(), "a2".into(), "a3".into()];
        view.request_line = Some("pedido sync enviado".into());
        let out = screen(60, 12, &view, &UiState::default());
        assert!(out.contains("pedido sync enviado"), "{out}");
    }

    #[test]
    fn no_state_and_degenerate_areas_do_not_panic() {
        let ui = UiState::default();
        let view = View::build(None, &ui, &cfg(), &UiFacts::default(), Utc::now());
        screen(110, 24, &view, &ui);
        screen(60, 12, &view, &ui);
        for (w, h) in [(0u16, 0u16), (1, 1), (60, 0), (0, 12)] {
            let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
            t.draw(|f| draw(f, &view, &ui)).unwrap();
        }
    }

    /// Gera `assets/tui.svg` para o README, com dados fictícios:
    /// `cargo test --lib readme_screenshot -- --ignored`
    #[test]
    #[ignore]
    fn readme_screenshot() {
        use crate::state::{Phase, ReviewsTodaySuccess};
        use ratatui::style::Color as C;
        let now = Utc::now();
        let mut s = State::default();
        s.repos.insert("acme/app".into(), RepoStatus { ok: true, last_sync: Some(now - chrono::Duration::seconds(20)), ..Default::default() });
        let pr = |n: u64, author: &str, title: &str, phase: Phase, age_min: i64| {
            let mut r = PrRecord::fixture("acme/app", n, "feat/x", "s");
            r.author = author.into();
            r.title = title.into();
            r.phase = phase;
            r.first_seen_at = now - chrono::Duration::minutes(age_min);
            r
        };
        s.insert(pr(4821, "marina", "feat: exporta relatório de fornecedores em CSV", Phase::Ready, 95));
        s.insert(pr(4830, "joao", "fix: valida CNPJ com dígito verificador zero", Phase::Ready, 60));
        s.insert(pr(4834, "bia", "refactor: extrai cliente HTTP do módulo de cotações", Phase::Preparing, 3));
        s.insert(pr(4835, "rafa", "test: cobre reprocessamento de webhooks", Phase::Failed { step: "pnpm install".into(), reason: "exit 1".into() }, 8));
        let mut leaving = pr(4812, "lu", "feat: filtro por categoria no painel", Phase::Ready, 300);
        leaving.remote = crate::state::Remote::NotPending;
        leaving.not_pending_since = Some(now - chrono::Duration::minutes(3));
        s.insert(leaving);
        let mut adopted = pr(4799, "voce", "feat: aprovação em lote de cadastros", Phase::Ready, 900);
        adopted.ownership = Ownership::Adopted { reason: "trocou para stack/lote-2".into(), at: now };
        s.insert(adopted);
        let day = crate::today::local_day(now);
        s.reviews_today.last_success = Some(ReviewsTodaySuccess {
            day, count: 6, per_repo: [("acme/app".to_string(), 6)].into(), as_of: now - chrono::Duration::minutes(2),
            viewer: "voce".into(), repos: vec!["acme/app".into()],
        });
        let cfg = ViewConfig { repos: vec!["acme/app".into()], poll_interval_secs: 60, remove_grace: chrono::Duration::minutes(15) };
        let ui = UiState::default();
        let mut view = View::build(Some(&s), &ui, &cfg, &UiFacts::default(), now);
        let ui = UiState { selected: view.order().first().cloned(), ..ui };
        view = View::build(Some(&s), &ui, &cfg, &UiFacts::default(), now);
        let (w, h) = (118u16, 20u16);
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, &view, &ui)).unwrap();
        let buf = t.backend().buffer().clone();

        let (fg0, bg0) = ("#d4d4d4", "#1e1e1e");
        let rgb = |c: C, def: &'static str| -> &'static str {
            match c {
                C::Reset => def,
                C::Black => "#1e1e1e",
                C::Red | C::LightRed => "#f14c4c",
                C::Green | C::LightGreen => "#23d18b",
                C::Yellow | C::LightYellow => "#e5c07b",
                C::Blue | C::LightBlue => "#3b8eea",
                C::Magenta | C::LightMagenta => "#c678dd",
                C::Cyan | C::LightCyan => "#29b8db",
                C::Gray => "#a0a0a0",
                C::DarkGray => "#767676",
                C::White => "#ffffff",
                _ => def,
            }
        };
        let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        let (cw, lh, pad) = (8.6f32, 19.0f32, 16.0f32);
        let (pw, ph) = (w as f32 * cw + 2.0 * pad, h as f32 * lh + 2.0 * pad + 28.0);
        let mut out = format!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{pw}\" height=\"{ph}\" viewBox=\"0 0 {pw} {ph}\">\n\
             <rect width=\"100%\" height=\"100%\" rx=\"10\" fill=\"{bg0}\"/>\n\
             <circle cx=\"20\" cy=\"16\" r=\"6\" fill=\"#ff5f56\"/><circle cx=\"40\" cy=\"16\" r=\"6\" fill=\"#ffbd2e\"/><circle cx=\"60\" cy=\"16\" r=\"6\" fill=\"#27c93f\"/>\n\
             <g font-family=\"ui-monospace,SFMono-Regular,Menlo,Consolas,monospace\" font-size=\"14\" xml:space=\"preserve\">\n"
        );
        let top = 28.0 + pad;
        for y in 0..h {
            let mut x = 0u16;
            while x < w {
                let c = &buf[(x, y)];
                let key = (c.fg, c.bg, c.modifier);
                let start = x;
                let mut text = String::new();
                while x < w && { let d = &buf[(x, y)]; (d.fg, d.bg, d.modifier) == key } {
                    text.push_str(buf[(x, y)].symbol());
                    x += 1;
                }
                let rev = key.2.contains(Modifier::REVERSED);
                let (mut fg, mut bg) = (rgb(key.0, fg0), rgb(key.1, bg0));
                if rev {
                    std::mem::swap(&mut fg, &mut bg);
                }
                let px = pad + start as f32 * cw;
                let py = top + y as f32 * lh;
                if bg != bg0 {
                    out += &format!("<rect x=\"{px}\" y=\"{}\" width=\"{}\" height=\"{lh}\" fill=\"{bg}\"/>\n", py - 14.0, (x - start) as f32 * cw);
                }
                // um <text> por trecho sem espaço, na coluna exata (renderizadores de SVG colapsam espaços)
                let weight = if key.2.contains(Modifier::BOLD) { " font-weight=\"bold\"" } else { "" };
                let cells: Vec<char> = text.chars().collect();
                let mut i = 0;
                while i < cells.len() {
                    if cells[i] == ' ' { i += 1; continue; }
                    let j = (i..cells.len()).find(|&k| cells[k] == ' ').unwrap_or(cells.len());
                    let seg: String = cells[i..j].iter().collect();
                    out += &format!(
                        "<text x=\"{}\" y=\"{py}\" fill=\"{fg}\"{weight} textLength=\"{}\" lengthAdjust=\"spacingAndGlyphs\">{}</text>\n",
                        px + i as f32 * cw,
                        (j - i) as f32 * cw,
                        esc(&seg)
                    );
                    i = j;
                }
            }
        }
        out += "</g>\n</svg>\n";
        std::fs::create_dir_all("assets").unwrap();
        std::fs::write("assets/tui.svg", out).unwrap();
        println!("{}", screen(w, h, &view, &ui));
    }
}
