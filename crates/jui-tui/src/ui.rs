use crate::app::{App, AssignPurpose, DetailFocus, DetailLinkedProject, MentionRole, Mode, PageLine, PendingDelete, TreeForm, TreeNode};
use jui_core::ticket::{fmt_date, fmt_seconds, parse_reply, priority_rank, Comment};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::layout::Alignment;
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1), Constraint::Length(2)])
        .split(f.area());
    draw_header(f, chunks[0], app);
    match &app.mode {
        Mode::List => draw_list(f, chunks[1], app),
        Mode::Archive => draw_archive(f, chunks[1], app),
        Mode::Kanban | Mode::KanbanFilter(_) => {
            draw_kanban(f, chunks[1], app);
            if matches!(&app.mode, Mode::KanbanFilter(_)) {
                draw_kanban_filter(f, chunks[1], app);
            }
        }
        Mode::Detail => draw_detail(f, chunks[1], app),
        Mode::Create(_) => draw_create(f, chunks[1], app),
        Mode::Edit(_) => draw_edit(f, chunks[1], app),
        Mode::Comment(_) => draw_comment(f, chunks[1], app),
        Mode::Transition(_) => draw_transition(f, chunks[1], app),
        Mode::EditTime(_) => draw_edit_time(f, chunks[1], app),
        Mode::EditPriority(_) => draw_edit_priority(f, chunks[1], app),
        Mode::StartWorkPrompt(_) => draw_start_work_prompt(f, chunks[1], app),
        Mode::Implementation(_) => draw_implementation(f, chunks[1], app),
        Mode::Projects(_) => draw_projects(f, chunks[1], app),
        Mode::ProjectsAdd(_) => draw_projects_add(f, chunks[1], app),
        Mode::TicketProjects(_) => draw_ticket_projects(f, chunks[1], app),
        Mode::ConfluenceSpaces(_) => draw_confluence_spaces(f, chunks[1], app),
        Mode::ConfluencePages(_) => draw_confluence_pages(f, chunks[1], app),
        Mode::PageView(_) => {
            let area = f.area();
            draw_page_view(f, area, app);
        }
        Mode::Tree(_) => draw_tree(f, chunks[1], app),
        Mode::AssignPicker(_) => {
            // Draw the underlying Detail first so the modal has context to overlay.
            draw_detail(f, chunks[1], app);
            draw_assign_picker(f, app);
        }
        Mode::ArchiveConfirm(_) => {
            draw_detail(f, chunks[1], app);
            draw_archive_confirm(f, app);
        }
    }
    if !matches!(&app.mode, Mode::PageView(_)) {
        draw_footer(f, chunks[2], app);
    }
    if app.show_help {
        draw_help_overlay(f, app);
    }
}

fn draw_header(f: &mut Frame, area: Rect, app: &App) {
    let conf_pages_label: String;
    let mode: &str = match &app.mode {
        Mode::List => "list",
        Mode::Archive => "archive",
        Mode::Kanban | Mode::KanbanFilter(_) => "kanban",
        Mode::Detail => "detail",
        Mode::Create(_) => "create",
        Mode::Edit(_) => "edit",
        Mode::Comment(_) => "comment",
        Mode::Transition(_) => "transition",
        Mode::EditTime(_) => "time",
        Mode::EditPriority(_) => "priority",
        Mode::StartWorkPrompt(_) => "start work",
        Mode::Implementation(_) => "implementation",
        Mode::Projects(_) => "projects",
        Mode::ProjectsAdd(_) => "projects/add",
        Mode::TicketProjects(_) => "ticket projects",
        Mode::PageView(form) => {
            conf_pages_label = format!("confluence / {}", form.title);
            &conf_pages_label
        }
        Mode::ConfluenceSpaces(_) => "confluence",
        Mode::Tree(_) => "tree",
        Mode::AssignPicker(form) => match form.purpose {
            AssignPurpose::Assignee => "assign",
            AssignPurpose::Reviewer => "reviewer",
        },
        Mode::ArchiveConfirm(_) => "archive?",
        Mode::ConfluencePages(form) => {
            conf_pages_label = if form.breadcrumb.is_empty() {
                format!("confluence / {}", form.space_name)
            } else {
                let crumb = form
                    .breadcrumb
                    .iter()
                    .map(|(_, t)| t.as_str())
                    .collect::<Vec<_>>()
                    .join(" › ");
                format!("confluence / {} › {}", form.space_name, crumb)
            };
            &conf_pages_label
        }
    };
    let title = format!(" jui — {}  |  {}", mode, app.status);
    let p = Paragraph::new(title).style(Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD));
    f.render_widget(p, area);
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    use crate::app::ListFocus;
    use ratatui::layout::{Constraint, Direction, Layout};
    // Split into Active (top) + Mentioned (bottom). Mentioned grows with row
    // count, capped at ~40% of the inner area so the active list still leads.
    let mentioned_rows = app.mentioned_tickets.len() as u16;
    let mentioned_h = if mentioned_rows == 0 {
        4 // "(none)" stub + border
    } else {
        (mentioned_rows + 2).clamp(5, (area.height / 2).max(5))
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(8),
            Constraint::Length(mentioned_h),
        ])
        .split(area);
    draw_list_active(f, chunks[0], app);
    draw_list_mentioned(f, chunks[1], app);
    let _ = ListFocus::Active; // imported for the helper fns below
}

fn draw_list_active(f: &mut Frame, area: Rect, app: &App) {
    use crate::app::ListFocus;
    let focused = app.list_focus == ListFocus::Active;
    // Grow key/status columns when the pane is wide so they don't butt up
    // against each other. Borders eat 2 cells; baseline target ≈ 80 cols.
    let extra = (area.width as usize).saturating_sub(2).saturating_sub(80);
    let key_grow = (extra / 6).min(6);
    let status_grow = (extra / 3).min(16);
    let mut prev_crumb: Option<String> = None;
    let items: Vec<ListItem> = app
        .active_idxs
        .iter()
        .enumerate()
        .filter_map(|(row_pos, i)| {
            let t = app.tickets.get(*i)?;
            let depth = app.active_row_depths.get(row_pos).copied().unwrap_or(0);
            let child_count = app.parent_child_counts.get(&t.key).copied().unwrap_or(0);
            let expanded = app.expanded_parents.contains(&t.key);

            let mut lines: Vec<Line> = Vec::new();
            // Breadcrumb only for top-of-window rows that are NOT children of a
            // visible row. Dedupe across consecutive top-level rows so the epic
            // prints once per group instead of repeating for every ticket.
            if depth == 0 {
                let crumb = breadcrumb_text(t);
                if crumb.is_some() && crumb != prev_crumb {
                    lines.push(breadcrumb_line_from_text(crumb.as_deref().unwrap()));
                }
                prev_crumb = crumb;
            } else {
                prev_crumb = None;
            }
            let mut spans: Vec<Span> = Vec::new();
            if depth > 0 {
                let indent = "  ".repeat(depth);
                spans.push(Span::styled(format!("{indent}└─ "), Style::default().fg(Color::DarkGray)));
            } else if child_count > 0 {
                let glyph = if expanded { "▾ " } else { "▸ " };
                let style = if expanded {
                    Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan)
                };
                spans.push(Span::styled(glyph.to_string(), style));
            } else {
                spans.push(Span::raw("  "));
            }

            let (glyph, glyph_style) = issue_type_glyph(t.issue_type.as_deref());
            spans.push(Span::styled(format!("{glyph} "), glyph_style));
            // The key column gets squeezed as depth grows so summary still has room.
            let key_width = (12_usize).saturating_sub(depth.saturating_mul(2)).max(8) + key_grow;
            spans.push(Span::styled(
                format!("{:<width$} ", t.key, width = key_width),
                Style::default().fg(Color::Yellow),
            ));
            let status_width = 14 + status_grow;
            spans.push(Span::styled(
                format!("{:<width$} ", truncate(&t.status, status_width), width = status_width),
                Style::default().fg(Color::Green),
            ));
            spans.push(priority_span(t.priority.as_deref()));
            spans.push(Span::raw(" "));
            spans.push(Span::raw(t.summary.clone()));

            if child_count > 0 {
                let glyph = if expanded { "▾" } else { "▸" };
                spans.push(Span::styled(
                    format!("  {glyph} {child_count}"),
                    Style::default().fg(Color::Cyan),
                ));
            }
            lines.push(Line::from(spans));
            Some(ListItem::new(lines))
        })
        .collect();
    let mut state = ListState::default();
    state.select(if app.active_idxs.is_empty() || !focused { None } else { Some(app.list_selected) });
    let title = format!(
        " active tickets — {} · sort: {} {}",
        app.active_idxs.len(),
        app.sort_mode.label(),
        if app.inactive_idxs.is_empty() {
            String::new()
        } else {
            format!(" ({} archived — 'a')", app.inactive_idxs.len())
        }
    );
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).border_style(focus_border(focused)).title(title))
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol(if focused { "▶ " } else { "  " });
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_list_mentioned(f: &mut Frame, area: Rect, app: &App) {
    use crate::app::ListFocus;
    let focused = app.list_focus == ListFocus::Mentioned;
    let combined = app.combined_mentions();
    let total = combined.len();
    let r = app.reviewing_tickets.len();
    let m = app.mentioned_tickets.len();
    let title = format!(
        " reviewing + mentioned — {r} reviewer · {m} @ (Shift+Tab to focus) "
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border(focused))
        .title(title);

    if combined.is_empty() {
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(
            Paragraph::new(Span::styled(
                " (none) — you're not a reviewer or @-mentioned on any open tickets",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            )),
            inner,
        );
        return;
    }

    let items: Vec<ListItem> = combined
        .iter()
        .map(|(role, t)| {
            let (badge, badge_style) = role_badge(*role);
            let (glyph, glyph_style) = issue_type_glyph(t.issue_type.as_deref());
            let line = Line::from(vec![
                Span::styled(format!(" {badge} "), badge_style),
                Span::styled(format!("{glyph} "), glyph_style),
                Span::styled(format!("{:<12} ", t.key), Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("{:<14} ", truncate(&t.status, 14)),
                    Style::default().fg(Color::Green),
                ),
                priority_span(t.priority.as_deref()),
                Span::raw(" "),
                Span::raw(t.summary.clone()),
            ]);
            ListItem::new(line)
        })
        .collect();
    let mut state = ListState::default();
    state.select(if focused && total > 0 { Some(app.mentioned_selected.min(total - 1)) } else { None });
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol(if focused { "▶ " } else { "  " });
    f.render_stateful_widget(list, area, &mut state);
}

/// Badge text + style for a role. `[A]` = assigned (rarely used since assigned
/// is the default in most renders), `[R]` = reviewer, `[@]` = mentioned.
fn role_badge(role: MentionRole) -> (&'static str, Style) {
    match role {
        MentionRole::Assigned => (
            "[A]",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ),
        MentionRole::Reviewer => (
            "[R]",
            Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD),
        ),
        MentionRole::Mentioned => (
            "[@]",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
    }
}

fn draw_archive(f: &mut Frame, area: Rect, app: &App) {
    let muted = Style::default().fg(Color::DarkGray);
    let items: Vec<ListItem> = app
        .inactive_idxs
        .iter()
        .filter_map(|i| app.tickets.get(*i))
        .map(|t| {
            let line = Line::from(vec![
                Span::styled(format!("{:<12}", t.key), muted),
                Span::styled(format!("{:<14}", truncate(&t.status, 14)), muted),
                Span::styled(t.summary.clone(), muted),
            ]);
            ListItem::new(line)
        })
        .collect();
    let mut state = ListState::default();
    state.select(if app.inactive_idxs.is_empty() { None } else { Some(app.archive_selected) });
    let title = format!(" archive — {} resolved/done/closed · sort: updated ", app.inactive_idxs.len());
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_kanban(f: &mut Frame, area: Rect, app: &App) {
    let cols = app.kanban_columns();
    let n = cols.len();
    let total = cols.iter().map(|(_, v)| v.len()).sum::<usize>();
    let title = if app.kanban_assignee_filter.is_empty() {
        format!(" kanban — {} tickets · {} columns ", total, n)
    } else {
        let mut names: Vec<&str> = app.kanban_assignee_filter.iter().map(|s| s.as_str()).collect();
        names.sort();
        format!(" kanban: {} — {} tickets ({} extra) · {} columns ", names.join(", "), total, app.kanban_extra.len(), n)
    };
    let outer = Block::default().borders(Borders::ALL).title(title);
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    if cols.is_empty() {
        let p = Paragraph::new("no active tickets").alignment(Alignment::Center);
        f.render_widget(p, inner);
        return;
    }

    // Expanded single-column mode: fill the whole inner area with one column.
    if let Some(exp_ci) = app.kanban_expanded_col {
        if let Some((status, idxs)) = cols.get(exp_ci) {
            let card_sel = app.kanban_card_per_col.get(exp_ci).copied().unwrap_or(0);
            draw_kanban_column_expanded(f, inner, app, status, idxs, card_sel);
        }
        return;
    }

    // Columns left of center minimise to the left edge; right of center to the right edge.
    let is_left_side = |i: usize| i * 2 < n;
    let left_min: Vec<usize> = (0..n)
        .filter(|&i| app.kanban_minimized.contains(&i) && is_left_side(i))
        .collect();
    let right_min: Vec<usize> = (0..n)
        .filter(|&i| app.kanban_minimized.contains(&i) && !is_left_side(i))
        .collect();
    let expanded_idxs: Vec<usize> = (0..n)
        .filter(|&i| !app.kanban_minimized.contains(&i))
        .collect();

    let strip_w: u16 = 3;
    let left_strips_w = left_min.len() as u16 * strip_w;
    let right_strips_w = right_min.len() as u16 * strip_w;
    let middle_w = inner.width.saturating_sub(left_strips_w + right_strips_w);

    // Draw minimised strips on the left edge.
    for (slot, &ci) in left_min.iter().enumerate() {
        let x = inner.x + slot as u16 * strip_w;
        let strip = Rect { x, y: inner.y, width: strip_w, height: inner.height };
        let (status, idxs) = &cols[ci];
        draw_minimized_column(f, strip, status, idxs.len(), ci == app.kanban_col);
    }

    // Draw minimised strips on the right edge.
    for (slot, &ci) in right_min.iter().enumerate() {
        let x = inner.x + left_strips_w + middle_w + slot as u16 * strip_w;
        let strip = Rect { x, y: inner.y, width: strip_w, height: inner.height };
        let (status, idxs) = &cols[ci];
        draw_minimized_column(f, strip, status, idxs.len(), ci == app.kanban_col);
    }

    // Draw expanded columns in the middle area with a sliding window.
    if expanded_idxs.is_empty() || middle_w == 0 {
        return;
    }
    let middle_area = Rect { x: inner.x + left_strips_w, y: inner.y, width: middle_w, height: inner.height };
    let min_col_w: u16 = 22;
    let max_visible = ((middle_w / min_col_w) as usize).max(1);
    let visible_n = expanded_idxs.len().min(max_visible);
    // Slide window to keep focused expanded column visible.
    let focused_pos = expanded_idxs.iter().position(|&i| i == app.kanban_col).unwrap_or(0);
    let start = if expanded_idxs.len() <= visible_n {
        0
    } else {
        let half = visible_n / 2;
        let max_start = expanded_idxs.len() - visible_n;
        focused_pos.saturating_sub(half).min(max_start)
    };
    let end = (start + visible_n).min(expanded_idxs.len());

    let constraints: Vec<Constraint> = (0..visible_n)
        .map(|_| Constraint::Ratio(1, visible_n as u32))
        .collect();
    let col_areas = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(middle_area);

    for (slot, &ci) in expanded_idxs[start..end].iter().enumerate() {
        let (status, idxs) = &cols[ci];
        let focused = ci == app.kanban_col;
        let card_sel = app.kanban_card_per_col.get(ci).copied().unwrap_or(0);
        draw_kanban_column(f, col_areas[slot], app, status, idxs, focused, card_sel);
    }
}

fn draw_minimized_column(f: &mut Frame, area: Rect, status: &str, count: usize, focused: bool) {
    let (border_color, text_color) = if focused {
        (Color::Cyan, Color::Cyan)
    } else {
        (Color::DarkGray, Color::Gray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if inner.height == 0 {
        return;
    }

    // Render status name vertically, one char per row.
    let chars: Vec<char> = status.chars().collect();
    let name_rows = inner.height.saturating_sub(if count > 0 { 1 } else { 0 }) as usize;
    for (row, ch) in chars.iter().take(name_rows).enumerate() {
        let cell = Rect { x: inner.x, y: inner.y + row as u16, width: 1, height: 1 };
        f.render_widget(
            Paragraph::new(Span::styled(ch.to_string(), Style::default().fg(text_color))),
            cell,
        );
    }

    // Count badge at bottom.
    if count > 0 && inner.height > 0 {
        let count_str = count.to_string();
        let cy = inner.y + inner.height - 1;
        let cell = Rect { x: inner.x, y: cy, width: 1.max(count_str.len() as u16).min(inner.width), height: 1 };
        f.render_widget(
            Paragraph::new(Span::styled(count_str, Style::default().fg(Color::Yellow))),
            cell,
        );
    }
}

fn draw_kanban_column(
    f: &mut Frame,
    area: Rect,
    app: &App,
    status: &str,
    idxs: &[usize],
    focused: bool,
    card_sel: usize,
) {
    let header_style = if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD)
    };
    let title = format!(" {} {} ", status.to_uppercase(), idxs.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        })
        .title(Span::styled(title, header_style));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if idxs.is_empty() {
        return;
    }

    // Card height is fixed at 5 rows (border + key/summary + summary line + meta + border).
    // We render cards stacked, scrolled so the selected card is visible.
    let card_h: u16 = 5;
    let avail_h = inner.height;
    let max_cards = (avail_h / card_h) as usize;
    if max_cards == 0 {
        return;
    }
    let total = idxs.len();
    let scroll_start = if focused {
        if card_sel >= max_cards {
            (card_sel + 1).saturating_sub(max_cards)
        } else {
            0
        }
    } else {
        0
    };
    let scroll_end = (scroll_start + max_cards).min(total);

    for (slot, i) in (scroll_start..scroll_end).enumerate() {
        let y = inner.y + (slot as u16) * card_h;
        let card_area = Rect { x: inner.x, y, width: inner.width, height: card_h };
        let selected = focused && i == card_sel;
        let Some(t) = app.kanban_ticket(idxs[i]) else { continue };
        draw_kanban_card(f, card_area, t, selected);
    }
}

fn draw_kanban_column_expanded(
    f: &mut Frame,
    area: Rect,
    app: &App,
    status: &str,
    idxs: &[usize],
    card_sel: usize,
) {
    let title = format!(" {} {} — expanded (e to collapse) ", status.to_uppercase(), idxs.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(title, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if idxs.is_empty() {
        return;
    }

    let card_h: u16 = 8;
    let max_cards = ((inner.height / card_h) as usize).max(1);
    let total = idxs.len();
    let scroll_start = if card_sel >= max_cards {
        (card_sel + 1).saturating_sub(max_cards)
    } else {
        0
    };
    let scroll_end = (scroll_start + max_cards).min(total);

    for (slot, i) in (scroll_start..scroll_end).enumerate() {
        let y = inner.y + (slot as u16) * card_h;
        let card_area = Rect { x: inner.x, y, width: inner.width, height: card_h };
        let selected = i == card_sel;
        let Some(t) = app.kanban_ticket(idxs[i]) else { continue };
        draw_kanban_card_expanded(f, card_area, t, selected);
    }
}

fn draw_kanban_card_expanded(f: &mut Frame, area: Rect, t: &jui_core::ticket::Ticket, selected: bool) {
    use jui_core::ticket::fmt_seconds;
    let border_style = if selected {
        Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default().borders(Borders::ALL).border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let w = inner.width as usize;

    // Row 1: key  type  parent summary
    let issue_type = t.issue_type.as_deref().unwrap_or("");
    let parent_budget = w.saturating_sub(t.key.len() + issue_type.len() + 4);
    let parent_text = truncate(t.parent_summary.as_deref().unwrap_or_default(), parent_budget);
    let key_line = Line::from(vec![
        Span::styled(t.key.clone(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(issue_type.to_string(), Style::default().fg(Color::Blue)),
        Span::raw(if issue_type.is_empty() { String::new() } else { "  ".to_string() }),
        Span::styled(parent_text, Style::default().fg(Color::DarkGray)),
    ]);

    // Rows 2-3: summary (allow wrapping across 2 rows)
    let summary_line = Line::from(Span::raw(truncate(&t.summary, w * 2)));

    // Row 4: priority + assignee
    let mut meta: Vec<Span> = vec![priority_dots(t.priority.as_deref())];
    if let Some(a) = t.assignee.as_deref().filter(|s| !s.is_empty()) {
        meta.push(Span::raw("  "));
        meta.push(Span::styled(
            format!("◉ {}", initials(a)),
            Style::default().fg(assignee_color(a)),
        ));
        // full name after initials
        meta.push(Span::styled(
            format!("  {}", a),
            Style::default().fg(Color::DarkGray),
        ));
    }

    // Row 5: estimate / time spent
    let est = t.original_estimate_seconds.map(|s| fmt_seconds(s));
    let spent = t.time_spent_seconds.map(|s| fmt_seconds(s));
    let time_line = match (est, spent) {
        (Some(e), Some(s)) => Line::from(Span::styled(
            format!("est {}  spent {}", e, s),
            Style::default().fg(Color::DarkGray),
        )),
        (Some(e), None) => Line::from(Span::styled(
            format!("est {}", e),
            Style::default().fg(Color::DarkGray),
        )),
        (None, Some(s)) => Line::from(Span::styled(
            format!("spent {}", s),
            Style::default().fg(Color::DarkGray),
        )),
        _ => Line::from(""),
    };

    // Row 6: labels
    let labels_line = if t.labels.is_empty() {
        Line::from("")
    } else {
        Line::from(Span::styled(
            truncate(&t.labels.join("  "), w),
            Style::default().fg(Color::Blue),
        ))
    };

    let lines = vec![
        key_line,
        summary_line,
        Line::from(""),
        Line::from(meta),
        time_line,
        labels_line,
    ];
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

fn draw_kanban_card(f: &mut Frame, area: Rect, t: &jui_core::ticket::Ticket, selected: bool) {
    let border_style = if selected {
        Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default().borders(Borders::ALL).border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }

    let parent_budget = (inner.width as usize).saturating_sub(t.key.len() + 2);
    let parent_text = truncate(
        t.parent_summary.as_deref().unwrap_or_default(),
        parent_budget,
    );
    let key_line = Line::from(vec![
        Span::styled(t.key.clone(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
        Span::styled(parent_text, Style::default().fg(Color::DarkGray)),
    ]);
    let summary_w = inner.width as usize;
    let summary_line = Line::from(Span::raw(truncate(&t.summary, summary_w)));
    let mut meta: Vec<Span> = Vec::new();
    meta.push(priority_dots(t.priority.as_deref()));
    if let Some(a) = t.assignee.as_deref().filter(|s| !s.is_empty()) {
        meta.push(Span::raw("  "));
        meta.push(Span::styled(
            format!("◉ {}", initials(a)),
            Style::default().fg(assignee_color(a)),
        ));
    }
    let lines = vec![key_line, summary_line, Line::from(meta)];
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

fn priority_dots(p: Option<&str>) -> Span<'static> {
    let rank = priority_rank(p);
    let (dots, color) = match rank {
        1 => ("●●●●●", Color::Red),
        2 => ("●●●●·", Color::LightRed),
        3 => ("●●●··", Color::Yellow),
        4 => ("●●···", Color::Cyan),
        5 => ("●····", Color::DarkGray),
        _ => ("·····", Color::DarkGray),
    };
    Span::styled(dots, Style::default().fg(color))
}

fn initials(name: &str) -> String {
    let mut out = String::new();
    for part in name.split_whitespace().take(2) {
        if let Some(ch) = part.chars().next() {
            out.push(ch.to_ascii_uppercase());
        }
    }
    if out.is_empty() {
        out.push_str("??");
    }
    out
}

/// Map an assignee display name to a consistent color from a distinct palette.
fn assignee_color(name: &str) -> Color {
    const PALETTE: &[Color] = &[
        Color::Magenta,
        Color::Green,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightGreen,
        Color::Cyan,
        Color::LightRed,
        Color::Blue,
    ];
    let hash = name.bytes().fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    PALETTE[(hash as usize) % PALETTE.len()]
}

fn draw_kanban_filter(f: &mut Frame, area: Rect, app: &App) {
    let Mode::KanbanFilter(form) = &app.mode else { return };

    let popup_w = 56_u16.min(area.width.saturating_sub(4));
    // Extra rows if teams exist or save prompt active.
    let save_rows: u16 = if form.save_name.is_some() { 2 } else { 0 };
    let team_rows: u16 = if form.teams.is_empty() { 0 } else { form.teams.len() as u16 + 1 }; // +1 separator
    let popup_h = (4 + team_rows + save_rows + 18).min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup_area = Rect { x, y, width: popup_w, height: popup_h };

    f.render_widget(ratatui::widgets::Clear, popup_area);

    let filter_label = if app.kanban_assignee_filter.is_empty() {
        " filter users ".to_string()
    } else {
        format!(" filter: {} selected ", app.kanban_assignee_filter.len())
    };
    let source_hint = if form.from_cache { " ·  ticket assignees only" } else { "" };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            format!("{}{}", filter_label, source_hint),
            Style::default().fg(if form.from_cache { Color::Yellow } else { Color::Cyan }).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup_area);
    f.render_widget(block, popup_area);

    if inner.height < 3 || inner.width < 4 {
        return;
    }

    let mut y_cursor = inner.y;
    let w = inner.width;
    let x0 = inner.x;

    // --- Teams section ---
    if !form.teams.is_empty() {
        let hdr = Paragraph::new(Span::styled(
            "  ★ TEAMS  (enter to apply · ctrl+d to delete)",
            Style::default().fg(Color::Yellow),
        ));
        f.render_widget(hdr, Rect { x: x0, y: y_cursor, width: w, height: 1 });
        y_cursor += 1;

        for (i, team) in form.teams.iter().enumerate() {
            let cursor = form.save_name.is_none() && i == form.selected;
            let member_str = team.members.join(", ");
            let label = truncate(&format!("★ {}  ({})", team.name, member_str), w as usize);
            let style = if cursor {
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD).bg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Yellow)
            };
            let p = Paragraph::new(Span::styled(label, style));
            f.render_widget(p, Rect { x: x0, y: y_cursor, width: w, height: 1 });
            y_cursor += 1;
        }

        // Separator line.
        let sep = Paragraph::new(Span::styled(
            "─".repeat(w as usize),
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(sep, Rect { x: x0, y: y_cursor, width: w, height: 1 });
        y_cursor += 1;
    }

    // --- Save-name input ---
    if let Some(ref name) = form.save_name {
        let prompt = Paragraph::new(Line::from(vec![
            Span::styled("  Save as team: ", Style::default().fg(Color::Cyan)),
            Span::styled(format!("{}_", name), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        ]));
        f.render_widget(prompt, Rect { x: x0, y: y_cursor, width: w, height: 1 });
        y_cursor += 1;
        let sep = Paragraph::new(Span::styled("─".repeat(w as usize), Style::default().fg(Color::DarkGray)));
        f.render_widget(sep, Rect { x: x0, y: y_cursor, width: w, height: 1 });
        y_cursor += 1;
    }

    // --- Search box ---
    let remaining_h = inner.y + inner.height - y_cursor;
    if remaining_h < 2 { return; }

    let search_area = Rect { x: x0, y: y_cursor, width: w, height: 1 };
    let list_area = Rect { x: x0, y: y_cursor + 1, width: w, height: remaining_h - 1 };

    let search_p = Paragraph::new(format!("/ {}_", form.query))
        .style(Style::default().fg(Color::White).add_modifier(Modifier::BOLD));
    f.render_widget(search_p, search_area);

    // Build user list, offset selection by teams count.
    let n_teams = form.teams.len();
    let user_sel = form.selected.saturating_sub(n_teams);
    let max_visible = list_area.height as usize;
    let scroll = if user_sel < max_visible { 0 } else { (user_sel + 1).saturating_sub(max_visible) };

    let items: Vec<ListItem> = form
        .results
        .iter()
        .skip(scroll)
        .take(max_visible)
        .enumerate()
        .map(|(slot, (name, _))| {
            let actual_user_idx = scroll + slot;
            let actual_row = n_teams + actual_user_idx;
            let checked = app.kanban_assignee_filter.contains(name);
            let cursor = form.save_name.is_none() && actual_row == form.selected;
            let check_style = if checked {
                Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let name_style = if cursor {
                Style::default().fg(Color::White).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let line = Line::from(vec![
                Span::styled(if checked { "● " } else { "○ " }, check_style),
                Span::styled(name.clone(), name_style),
            ]);
            ListItem::new(line)
        })
        .collect();

    let mut state = ListState::default();
    let visible_sel = user_sel.saturating_sub(scroll);
    state.select(if form.results.is_empty() || form.selected < n_teams || form.save_name.is_some() {
        None
    } else {
        Some(visible_sel)
    });
    let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));
    f.render_stateful_widget(list, list_area, &mut state);
}

fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let outer = Block::default().borders(Borders::ALL).title(" detail · tab to switch panes ");
    let inner = outer.inner(area);
    f.render_widget(outer, area);
    let Some(t) = &app.detail else {
        let p = Paragraph::new("loading…");
        f.render_widget(p, inner);
        return;
    };

    // Stacked panes: info, linked projects, [subtasks], comments. Hide the subtasks
    // pane when this ticket is itself a sub-task (Jira disallows nested sub-tasks).
    let is_subtask = t
        .issue_type
        .as_deref()
        .map(|x| x.eq_ignore_ascii_case("sub-task") || x.eq_ignore_ascii_case("subtask"))
        .unwrap_or(false);
    let projects_h: u16 = ((app.detail_linked_projects.len() as u16).max(1) + 2).min(7);
    // Visible subtask count (after the archived filter) drives the pane height when
    // the user isn't actively focused on subtasks.
    let visible_subtasks = visible_subtask_count(app, t);
    let subtasks_h: u16 = if app.detail_focus == DetailFocus::Subtasks {
        (inner.height / 2).max(7)
    } else {
        ((visible_subtasks as u16).max(1) + 2).min(7)
    };
    // Comments pane: shrink to a 3-row stub when empty AND unfocused so the freed
    // rows go to subtasks/info. Stays focusable so 'c' still works.
    let comments_focused = app.detail_focus == DetailFocus::Comments;
    let comments_collapsed = app.comments.is_empty() && !comments_focused;
    let comments_constraint = if comments_collapsed {
        Constraint::Length(3)
    } else {
        Constraint::Min(5)
    };
    if is_subtask {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(8),
                Constraint::Length(projects_h),
                comments_constraint,
            ])
            .split(inner);
        draw_detail_info(f, chunks[0], app, t);
        draw_detail_projects(f, chunks[1], app);
        draw_detail_comments(f, chunks[2], app);
    } else {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(8),
                Constraint::Length(projects_h),
                Constraint::Length(subtasks_h),
                comments_constraint,
            ])
            .split(inner);
        draw_detail_info(f, chunks[0], app, t);
        draw_detail_projects(f, chunks[1], app);
        draw_detail_subtasks(f, chunks[2], app, t);
        draw_detail_comments(f, chunks[3], app);
    }
}

fn visible_subtask_count(app: &App, t: &jui_core::ticket::Ticket) -> usize {
    if app.show_archived_subtasks { t.subtasks.len() }
    else { t.subtasks.iter().filter(|s| !is_subtask_archived(s)).count() }
}

fn is_subtask_archived(s: &jui_core::ticket::SubtaskRef) -> bool {
    let status = s.status.as_deref().unwrap_or("").to_ascii_lowercase();
    matches!(status.as_str(), "resolved" | "done" | "closed" | "archive" | "archived" | "won't do" | "wont do" | "cancelled" | "canceled")
}

fn draw_detail_subtasks(f: &mut Frame, area: Rect, app: &App, t: &jui_core::ticket::Ticket) {
    let focused = app.detail_focus == DetailFocus::Subtasks;
    let visible_idxs = crate::app::visible_subtask_indices(app);
    let total = t.subtasks.len();
    let visible = visible_idxs.len();
    let hidden = total.saturating_sub(visible);
    let title = if hidden > 0 {
        format!(" subtasks ({visible} of {total} · {hidden} hidden — A toggle) ")
    } else {
        format!(" subtasks ({visible}) ")
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border(focused))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if visible == 0 {
        let hint = if total > 0 {
            "(all archived — press A to show)".to_string()
        } else if focused {
            "(none) · press 'a' or 'T' to add".into()
        } else {
            "(none) · tab into pane, then 'a' to add".into()
        };
        let p = Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray)));
        f.render_widget(p, inner);
        return;
    }

    let items: Vec<ListItem> = visible_idxs
        .iter()
        .map(|&i| &t.subtasks[i])
        .map(|s| {
            let status = s.status.as_deref().unwrap_or("?");
            let inactive = matches!(
                status.to_ascii_lowercase().as_str(),
                "resolved" | "done" | "closed"
            );
            let key_style = if inactive {
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)
            } else {
                Style::default().fg(Color::Yellow)
            };
            let status_style = if inactive {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Green)
            };
            let summary_style = if inactive {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            let (glyph, glyph_style) = issue_type_glyph(s.issue_type.as_deref());
            let line = Line::from(vec![
                Span::raw(" "),
                Span::styled(format!("{glyph} "), glyph_style),
                Span::styled(format!("{:<11}", s.key), key_style),
                Span::styled(format!("{:<14}", truncate(status, 14)), status_style),
                Span::styled(s.summary.clone(), summary_style),
            ]);
            ListItem::new(line)
        })
        .collect();
    let mut state = ListState::default();
    if focused && visible > 0 {
        state.select(Some(app.subtask_selected.min(visible - 1)));
    }
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol(if focused { "▶ " } else { "  " });
    f.render_stateful_widget(list, inner, &mut state);
}

fn focus_border(focused: bool) -> Style {
    if focused {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn draw_detail_info(f: &mut Frame, area: Rect, app: &App, t: &jui_core::ticket::Ticket) {
    let focused = app.detail_focus == DetailFocus::Info;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border(focused))
        .title(" info ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(vec![
            Span::styled(t.key.clone(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(t.status.clone(), Style::default().fg(Color::Green)),
            Span::raw("  "),
            Span::styled(t.priority.clone().unwrap_or_default(), Style::default().fg(Color::Magenta)),
        ]),
        Line::from(Span::styled(
            t.summary.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        {
            let (glyph, gstyle) = issue_type_glyph(t.issue_type.as_deref());
            Line::from(vec![
                Span::raw("type: "),
                Span::styled(format!("{glyph} "), gstyle),
                Span::raw(t.issue_type.clone().unwrap_or_default()),
                Span::raw("  assignee: "),
                Span::raw(t.assignee.clone().unwrap_or_else(|| "—".into())),
                Span::raw("  reporter: "),
                Span::raw(t.reporter.clone().unwrap_or_else(|| "—".into())),
            ])
        },
        Line::from(vec![
            Span::styled("time:  ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(
                "estimate {} · remaining {} · logged {}  ",
                t.original_estimate_seconds.map(fmt_seconds).unwrap_or_else(|| "—".into()),
                t.remaining_estimate_seconds.map(fmt_seconds).unwrap_or_else(|| "—".into()),
                t.time_spent_seconds.map(fmt_seconds).unwrap_or_else(|| "—".into()),
            )),
            Span::styled("(w to edit)", Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(vec![
            Span::styled("dates: ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(
                "created {} · updated {}",
                t.created.as_deref().map(fmt_date).unwrap_or_else(|| "—".into()),
                t.updated.as_deref().map(fmt_date).unwrap_or_else(|| "—".into()),
            )),
        ]),
        Line::from(""),
    ];
    if let Some(d) = &t.description {
        for ln in d.lines().take(20) {
            lines.push(Line::from(ln.to_string()));
        }
    }
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

fn draw_detail_projects(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.detail_focus == DetailFocus::Projects;
    let title = format!(" linked projects ({}) ", app.detail_linked_projects.len());
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border(focused))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.detail_linked_projects.is_empty() {
        let hint = if focused {
            "(none) · press 'a' to add"
        } else {
            "(none) · tab into pane, then 'a' to add"
        };
        let p = Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray)));
        f.render_widget(p, inner);
        return;
    }

    let pending = match &app.pending_delete {
        Some(PendingDelete::Link(p)) => Some(p.clone()),
        _ => None,
    };

    let items: Vec<ListItem> = app
        .detail_linked_projects
        .iter()
        .map(|p| project_link_item(p, pending.as_ref() == Some(&p.project.path)))
        .collect();
    let mut state = ListState::default();
    if focused && !app.detail_linked_projects.is_empty() {
        state.select(Some(app.linked_project_selected.min(app.detail_linked_projects.len() - 1)));
    }
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol(if focused { "▶ " } else { "  " });
    f.render_stateful_widget(list, inner, &mut state);
}

fn project_link_item(item: &DetailLinkedProject, pending_unlink: bool) -> ListItem<'_> {
    let p = &item.project;
    let suggested = item.state == "suggested";
    let no_match = item.state == "no_match";
    if no_match {
        let spans = vec![
            Span::styled(" ~ ", Style::default().fg(Color::DarkGray).add_modifier(Modifier::BOLD)),
            Span::styled(
                "claude found no clear match",
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
            ),
            Span::styled(
                "  (d to dismiss this notice)",
                Style::default().fg(Color::DarkGray),
            ),
        ];
        return ListItem::new(Line::from(spans));
    }
    let (marker, mstyle) = if suggested {
        ("?", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
    } else if !p.available {
        ("✗", Style::default().fg(Color::Red))
    } else if p.kind == "git" {
        ("●", Style::default().fg(Color::Green))
    } else {
        ("●", Style::default().fg(Color::Cyan))
    };
    let label = p
        .nickname
        .clone()
        .or_else(|| p.path.file_name().and_then(|n| n.to_str()).map(str::to_string))
        .unwrap_or_else(|| p.path.display().to_string());
    let label_style = if suggested {
        Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let path_style = if suggested {
        Style::default().fg(Color::Yellow)
    } else if p.available {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)
    };
    let mut spans = vec![
        Span::styled(format!(" {marker} "), mstyle),
        Span::styled(format!("{label:<20}"), label_style),
        Span::styled(p.path.display().to_string(), path_style),
    ];
    if suggested {
        spans.push(Span::styled(
            "  (suggested by claude — y to keep, d to dismiss)",
            Style::default().fg(Color::Yellow),
        ));
    }
    if !p.available && !suggested {
        spans.push(Span::styled("  (unavailable)", Style::default().fg(Color::Red)));
    }
    if pending_unlink {
        spans.push(Span::styled(
            "  press 'd' again to unlink",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn draw_detail_comments(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.detail_focus == DetailFocus::Comments;
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border(focused))
        .title(format!(" comments ({}) ", app.comments.len()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if app.comments.is_empty() {
        let p = Paragraph::new(Span::styled(
            "(no comments) · press 'c' to add the first one",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(p, inner);
        return;
    }
    // Reuse the existing list-rendering helper. It expects `area` rather than `inner`,
    // so call draw_comments with the inner rect.
    draw_comments(f, inner, app);
}

fn draw_comments(f: &mut Frame, area: Rect, app: &App) {
    if app.comments.is_empty() {
        let p = Paragraph::new(Span::styled(
            "(no comments)  press 'c' to add the first one",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(p, area);
        return;
    }
    let body_width = (area.width as usize).saturating_sub(4); // borders + padding
    let items: Vec<ListItem> = app
        .comments
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mine = app.comment_is_mine(c);
            let pending = mine
                && i == app.comment_selected
                && matches!(
                    &app.pending_delete,
                    Some(PendingDelete::Comment(p))
                        if Some(p.as_str()) == c.id.as_deref()
                );
            comment_item(c, body_width, mine, pending)
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.comment_selected.min(app.comments.len() - 1)));
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::Rgb(40, 40, 60)))
        .highlight_symbol("");
    f.render_stateful_widget(list, area, &mut state);
}

fn comment_item<'a>(c: &'a Comment, width: usize, mine: bool, pending_delete: bool) -> ListItem<'a> {
    let date = fmt_date(&c.created);
    let parsed = parse_reply(&c.body);

    let mine_tag: Vec<Span> = if mine {
        let style = if pending_delete {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Green)
        };
        let label = if pending_delete { "  (press 'd' again to delete)" } else { "  (you)" };
        vec![Span::styled(label, style)]
    } else {
        vec![]
    };

    let (header_line, indent, body_text): (Line, &str, &str) = if let Some(r) = &parsed {
        // Reply: indented header with "↳" and reference to whom we're replying.
        let mut spans = vec![
            Span::styled("  ↳ ", Style::default().fg(Color::Cyan)),
            Span::styled(
                c.author.clone(),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  {date}  ")),
            Span::styled(
                format!("(re: {})", r.quoted_author),
                Style::default().fg(Color::Cyan),
            ),
        ];
        spans.extend(mine_tag.clone());
        (Line::from(spans), "    ", r.reply_text)
    } else {
        let mut spans = vec![
            Span::styled(
                c.author.clone(),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!("  {date}")),
        ];
        spans.extend(mine_tag.clone());
        (Line::from(spans), "  ", c.body.as_str())
    };

    let mut lines = vec![header_line];
    let inner_width = width.saturating_sub(indent.len()).max(20);
    for ln in wrap_text(body_text, inner_width) {
        lines.push(Line::from(format!("{indent}{ln}")));
    }
    lines.push(Line::from(""));
    ListItem::new(lines)
}

/// Greedy word-wrap that preserves explicit newlines.
fn wrap_text(s: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    for line in s.split('\n') {
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        for word in line.split_whitespace() {
            if cur.is_empty() {
                cur = word.to_string();
            } else if cur.chars().count() + 1 + word.chars().count() <= width {
                cur.push(' ');
                cur.push_str(word);
            } else {
                out.push(std::mem::take(&mut cur));
                cur = word.to_string();
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

fn draw_create(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Create(form) = &app.mode else { return };
    let mut lines: Vec<Line> = Vec::new();
    if let Some(parent) = &form.parent {
        lines.push(Line::from(vec![
            Span::styled("↳ subtask of ", Style::default().fg(Color::Cyan)),
            Span::styled(
                parent.clone(),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));
    }
    lines.push(field_line("project    ", &form.project, form.field == 0));
    lines.push(field_line("type       ", &form.issue_type, form.field == 1));
    lines.push(field_line("summary    ", &form.summary, form.field == 2));
    lines.push(field_line("description", &form.description, form.field == 3));
    lines.push(field_line("estimate   ", &form.time_estimate, form.field == 4));
    if form.field == 4 {
        lines.push(Line::from(Span::styled(
            "  examples: 8h, 2d 4h, 30m  (leave blank to skip)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(field_line("priority   ", &form.priority, form.field == 5));
    if form.field == 5 {
        lines.push(Line::from(Span::styled(
            "  examples: Highest, High, Medium, Low, Lowest  (leave blank to skip)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines.push(field_line("assignee   ", &form.assignee, form.field == 6));
    if form.field == 6 {
        if form.assignee_results.is_empty() {
            lines.push(Line::from(Span::styled(
                "  type ≥ 2 chars to search users  (blank = me)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for (i, (name, _id)) in form.assignee_results.iter().enumerate().take(8) {
                let style = if i == form.assignee_picker_selected {
                    Style::default().bg(Color::Rgb(60, 60, 80)).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let prefix = if i == form.assignee_picker_selected { "  ▶ " } else { "    " };
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_string(), Style::default().fg(Color::DarkGray)),
                    Span::styled(name.clone(), style),
                ]));
            }
            lines.push(Line::from(Span::styled(
                "  ↑/↓ navigate · enter: pick",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    if let Some(err) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "── error ────────────────────────────────────────────────",
            Style::default().fg(Color::Red),
        )));
        for ln in err.lines() {
            lines.push(Line::from(Span::styled(
                ln.to_string(),
                Style::default().fg(Color::Red),
            )));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "tab/shift+tab: switch   enter: next/submit   F5 or ctrl+enter or ctrl+s: submit anywhere   esc: cancel",
        Style::default().fg(Color::DarkGray),
    )));
    let title = if form.parent.is_some() { " create subtask " } else { " create " };
    let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(p, area);
}

fn draw_edit(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Edit(form) = &app.mode else { return };
    let lines = vec![
        Line::from(format!("editing {}", form.key)),
        Line::from(""),
        field_line("summary   ", &form.summary, true),
        Line::from(""),
        Line::from(Span::styled("enter: save   esc: cancel", Style::default().fg(Color::DarkGray))),
    ];
    let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" edit "));
    f.render_widget(p, area);
}

fn draw_comment(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Comment(form) = &app.mode else { return };
    let title = if form.reply_to.is_some() {
        format!(" reply on {} ", form.key)
    } else {
        format!(" comment on {} ", form.key)
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // If this is a reply, show the parent excerpt up top so the user has context.
    let (top_h, has_quote) = if form.reply_to.is_some() { (4, true) } else { (1, false) };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(top_h), Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    if has_quote {
        let ctx = form.reply_to.as_ref().unwrap();
        let excerpt = ctx.parent_body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let lines = vec![
            Line::from(vec![
                Span::styled("↳ replying to ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    ctx.parent_author.clone(),
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {}", fmt_date(&ctx.parent_date))),
            ]),
            Line::from(Span::styled(
                format!("  > {}", truncate(excerpt, 100)),
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(""),
        ];
        let p = Paragraph::new(lines).wrap(Wrap { trim: false });
        f.render_widget(p, chunks[0]);
    } else {
        let header = Paragraph::new(Span::styled(
            "new comment",
            Style::default().add_modifier(Modifier::BOLD),
        ));
        f.render_widget(header, chunks[0]);
    }

    let body_with_cursor = format!("{}▏", form.body);
    let body = Paragraph::new(body_with_cursor).wrap(Wrap { trim: false });
    f.render_widget(body, chunks[1]);

    let hint = Paragraph::new(Span::styled(
        "ctrl+s: submit   esc: cancel",
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(hint, chunks[2]);
}

fn draw_ticket_projects(f: &mut Frame, area: Rect, app: &App) {
    let Mode::TicketProjects(form) = &app.mode else { return };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" link projects to {} ", form.ticket_key));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if let Some(err) = &form.error {
        let p = Paragraph::new(format!("{err}")).style(Style::default().fg(Color::Red));
        f.render_widget(p, inner);
        return;
    }
    if form.items.is_empty() {
        let p = Paragraph::new("loading…").style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, inner);
        return;
    }

    let items: Vec<ListItem> = form
        .items
        .iter()
        .map(|item| {
            let check = if item.linked { "[x]" } else { "[ ]" };
            let check_style = if item.linked {
                Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let path_style = if item.project.available {
                Style::default()
            } else {
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)
            };
            let mut spans = vec![
                Span::styled(format!(" {check} "), check_style),
                Span::styled(format!("{:<3}", item.project.kind), Style::default().fg(Color::DarkGray)),
                Span::raw("  "),
                Span::styled(item.project.path.display().to_string(), path_style),
            ];
            if let Some(nick) = &item.project.nickname {
                spans.push(Span::styled(format!("  ({nick})"), Style::default().fg(Color::DarkGray)));
            }
            if !item.project.available {
                spans.push(Span::styled("  (unavailable)", Style::default().fg(Color::Red)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(form.selected.min(form.items.len() - 1)));
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, inner, &mut state);
}

fn draw_projects(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Projects(form) = &app.mode else { return };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" projects ({} configured) ", form.items.len()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if form.items.is_empty() {
        let p = Paragraph::new(Span::styled(
            "no projects configured · press 'a' to add one",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(p, inner);
        return;
    }

    let items: Vec<ListItem> = form
        .items
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let pending = form.pending_remove.as_deref() == Some(p.path.as_path()) && i == form.selected;
            let (marker, marker_style) = if !p.available {
                ("✗", Style::default().fg(Color::Red))
            } else if p.kind == "git" {
                ("●", Style::default().fg(Color::Green))
            } else {
                ("●", Style::default().fg(Color::Cyan))
            };
            let kind = format!("{:<3}", p.kind);
            let path_style = if !p.available {
                Style::default().fg(Color::DarkGray).add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            let mut spans = vec![
                Span::styled(format!(" {marker} "), marker_style),
                Span::styled(kind, Style::default().fg(Color::DarkGray)),
                Span::raw("  "),
                Span::styled(p.path.display().to_string(), path_style),
            ];
            if let Some(nick) = &p.nickname {
                spans.push(Span::styled(format!("  ({nick})"), Style::default().fg(Color::DarkGray)));
            }
            if !p.available {
                spans.push(Span::styled("  (unavailable)", Style::default().fg(Color::Red)));
            }
            if pending {
                spans.push(Span::styled(
                    "  press 'd' again to remove",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(form.selected.min(form.items.len() - 1)));
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, inner, &mut state);
}

fn draw_projects_add(f: &mut Frame, area: Rect, app: &App) {
    let Mode::ProjectsAdd(form) = &app.mode else { return };
    let block = Block::default().borders(Borders::ALL).title(" add project ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let header = Paragraph::new(Line::from(vec![
        Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}▏", form.query),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
    ]));
    f.render_widget(header, chunks[0]);

    if form.loading {
        let p = Paragraph::new("scanning $HOME for repos…").style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, chunks[1]);
    } else if let Some(err) = &form.error {
        let p = Paragraph::new(format!("error: {err}")).style(Style::default().fg(Color::Red));
        f.render_widget(p, chunks[1]);
    } else {
        let filtered = form.filtered();
        let items: Vec<ListItem> = filtered
            .iter()
            .filter_map(|i| form.repos.get(*i))
            .map(|r| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {:<3}", r.kind), Style::default().fg(Color::Cyan)),
                    Span::raw(r.path.display().to_string()),
                ]))
            })
            .collect();
        let mut state = ListState::default();
        state.select(if items.is_empty() { None } else { Some(form.selected.min(items.len() - 1)) });
        let list = List::new(items)
            .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("▶ ");
        f.render_stateful_widget(list, chunks[1], &mut state);
    }

    let footer = Paragraph::new(Span::styled(
        "type to filter   j/k move   enter add   esc back",
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(footer, chunks[2]);
}

fn draw_implementation(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Implementation(form) = &app.mode else { return };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" claude implementation — {} ", form.key));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let projects = if form.project_paths.is_empty() {
        "—".to_string()
    } else {
        form.project_paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(" · ")
    };
    let header_text = format!("projects: {projects}\nupdated: {}", if form.updated_at.is_empty() { "—".into() } else { form.updated_at.clone() });
    let header = Paragraph::new(header_text).style(Style::default().fg(Color::DarkGray));
    f.render_widget(header, chunks[0]);

    let body = if form.markdown.is_empty() {
        Paragraph::new(Span::styled(
            "(no implementation yet — generation has been queued in the background)",
            Style::default().fg(Color::DarkGray),
        ))
        .wrap(Wrap { trim: false })
        .scroll((form.scroll, 0))
    } else {
        let lines = crate::md::render_markdown(&form.markdown);
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((form.scroll, 0))
    };
    f.render_widget(body, chunks[1]);

    let hint = Paragraph::new(Span::styled(
        if form.status_line.is_empty() {
            "j/k scroll · s save markdown · o launch claude in tmux · r reload · R regenerate · esc back".into()
        } else {
            form.status_line.clone()
        },
        Style::default().fg(Color::DarkGray),
    ));
    f.render_widget(hint, chunks[2]);
}

fn draw_start_work_prompt(f: &mut Frame, area: Rect, app: &App) {
    let Mode::StartWorkPrompt(form) = &app.mode else { return };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" start work — {} ", form.ticket_key));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(Span::styled(
            "before launching claude, please fill in the missing field(s):",
            Style::default().fg(Color::Cyan),
        )),
        Line::from(""),
    ];
    if form.need_time {
        lines.push(field_line("estimate  ", &form.time_estimate, form.field == 0));
        if form.field == 0 {
            lines.push(Line::from(Span::styled(
                "  examples: 8h, 2d 4h, 30m",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    if form.need_priority {
        lines.push(field_line("priority  ", &form.priority, form.field == 1));
        if form.field == 1 {
            let hint = if form.valid_priorities.is_empty() {
                "  examples: Highest, High, Medium, Low, Lowest".to_string()
            } else {
                format!("  valid: {}", form.valid_priorities.join(", "))
            };
            lines.push(Line::from(Span::styled(
                hint,
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    if let Some(err) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(Color::Red),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "tab: switch field   enter / F5 / ctrl+s: submit & continue   esc: cancel",
        Style::default().fg(Color::DarkGray),
    )));
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

fn draw_edit_priority(f: &mut Frame, area: Rect, app: &App) {
    let Mode::EditPriority(form) = &app.mode else { return };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" priority — {} ", form.key));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if form.loading {
        f.render_widget(Paragraph::new("loading priorities…"), inner);
        return;
    }
    if let Some(err) = &form.error {
        let p = Paragraph::new(format!("error: {err}")).style(Style::default().fg(Color::Red));
        f.render_widget(p, inner);
        return;
    }
    let items: Vec<ListItem> = form
        .options
        .iter()
        .map(|name| {
            ListItem::new(Line::from(vec![priority_span(Some(name)), Span::raw(name.clone())]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(if form.options.is_empty() { None } else { Some(form.selected) });
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, inner, &mut state);
}

fn draw_edit_time(f: &mut Frame, area: Rect, app: &App) {
    let Mode::EditTime(form) = &app.mode else { return };
    let lines = vec![
        Line::from(format!("time tracking — {}", form.key)),
        Line::from(""),
        field_line("estimate  ", &form.original_estimate, form.field == 0),
        Line::from(Span::styled(
            "  examples: 8h, 2d 4h, 30m  (leave blank to skip)",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        field_line("log work  ", &form.log_work, form.field == 1),
        Line::from(Span::styled(
            "  examples: 30m, 1h 30m  (leave blank to skip)",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "tab: switch field   enter: submit   esc: cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" time "));
    f.render_widget(p, area);
}

fn draw_transition(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Transition(form) = &app.mode else { return };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" transition {} ", form.key));
    let inner = block.inner(area);
    f.render_widget(block, area);

    if form.loading {
        let p = Paragraph::new("loading transitions…").alignment(Alignment::Left);
        f.render_widget(p, inner);
        return;
    }
    if let Some(err) = &form.error {
        let p = Paragraph::new(format!("error: {err}"))
            .style(Style::default().fg(Color::Red));
        f.render_widget(p, inner);
        return;
    }

    let items: Vec<ListItem> = form
        .options
        .iter()
        .map(|t| {
            let to = t.to_status.clone().unwrap_or_default();
            let line = Line::from(vec![
                Span::styled(format!("{:<24}", t.name), Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(format!("→ {to}"), Style::default().fg(Color::Green)),
            ]);
            ListItem::new(line)
        })
        .collect();
    let mut state = ListState::default();
    state.select(if form.options.is_empty() { None } else { Some(form.selected) });
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, inner, &mut state);
}

type Hint = (&'static str, &'static str);

fn draw_footer(f: &mut Frame, area: Rect, app: &App) {
    let hints = mode_hints(app);
    let p = Paragraph::new(render_hints(&hints));
    f.render_widget(p, area);
}

fn mode_hints(app: &App) -> Vec<Hint> {
    match &app.mode {
        Mode::List => vec![
            ("j/k", "move"),
            ("tab", "expand subtasks"),
            ("S-tab", "toggle section"),
            ("enter", "open"),
            ("r", "refresh"),
            ("o", "sort"),
            ("n", "new"),
            ("s", "start"),
            ("T", "tree"),
            ("a", "archive"),
            ("b", "board"),
            ("p", "projects"),
            ("f", "confluence"),
            ("q", "quit"),
        ],
        Mode::Kanban => vec![
            ("h/l", "column"),
            ("j/k", "card"),
            ("enter", "open"),
            ("e", "expand col"),
            ("m", "minimize col"),
            ("u", "filter users"),
            ("r", "refresh"),
            ("b/esc", "back"),
        ],
        Mode::KanbanFilter(ref form) => if form.save_name.is_some() {
            vec![("type", "team name"), ("enter", "save"), ("esc", "cancel")]
        } else {
            let mut hints = vec![
                ("type", "search"),
                ("j/k", "move"),
                ("space/enter", "toggle"),
                ("ctrl+c", "clear all"),
                ("esc", "done"),
            ];
            if !app.kanban_assignee_filter.is_empty() {
                hints.push(("ctrl+s", "save team"));
            }
            hints
        },
        Mode::Projects(_) => vec![
            ("j/k", "move"),
            ("a", "add"),
            ("d", "remove"),
            ("r", "refresh"),
            ("esc", "back"),
        ],
        Mode::ProjectsAdd(_) => vec![
            ("type", "filter"),
            ("j/k", "move"),
            ("enter", "add"),
            ("esc", "back"),
        ],
        Mode::Archive => vec![
            ("j/k", "move"),
            ("enter", "open"),
            ("r", "refresh"),
            ("o", "sort"),
            ("a/esc", "back"),
        ],
        Mode::Detail => match app.detail_focus {
            DetailFocus::Info => {
                let s_label = match app.detail.as_ref() {
                    Some(t) if crate::app::is_ticket_started(t) => "stop",
                    _ => "start",
                };
                let is_subtask = app
                    .detail
                    .as_ref()
                    .and_then(|t| t.issue_type.as_deref())
                    .map(|x| x.eq_ignore_ascii_case("sub-task") || x.eq_ignore_ascii_case("subtask"))
                    .unwrap_or(false);
                let mut v: Vec<Hint> = vec![
                    ("tab", "pane"),
                    ("e", "edit"),
                    ("c", "comment"),
                    ("t", "trans"),
                    ("w", "time"),
                    ("i", "prio"),
                    ("P", "link"),
                ];
                if !is_subtask {
                    v.push(("T", "subtask"));
                }
                v.extend([
                    ("@", "assign"),
                    ("R", "reviewer"),
                    ("C", "claude"),
                    ("s", s_label),
                    ("D", "archive"),
                    ("esc", "back"),
                ]);
                v
            }
            DetailFocus::Projects => {
                let has_suggestion = app
                    .detail_linked_projects
                    .iter()
                    .any(|p| p.state == "suggested");
                if has_suggestion {
                    vec![
                        ("tab", "next pane"),
                        ("j/k", "move"),
                        ("a", "add"),
                        ("y", "approve"),
                        ("d", "dismiss/unlink"),
                        ("C", "claude"),
                        ("esc", "back"),
                    ]
                } else {
                    vec![
                        ("tab", "next pane"),
                        ("j/k", "move"),
                        ("a", "add"),
                        ("d", "unlink"),
                        ("C", "claude"),
                        ("esc", "back"),
                    ]
                }
            }
            DetailFocus::Subtasks => vec![
                ("tab", "next pane"),
                ("j/k", "move"),
                ("enter", "open"),
                ("a", "add subtask"),
                ("A", "toggle archived"),
                ("D", "archive"),
                ("C", "claude"),
                ("esc", "back"),
            ],
            DetailFocus::Comments => vec![
                ("tab", "next pane"),
                ("j/k", "move"),
                ("c", "new"),
                ("R", "reply"),
                ("d", "delete (own)"),
                ("C", "claude"),
                ("esc", "back"),
            ],
        },
        Mode::TicketProjects(_) => vec![
            ("j/k", "move"),
            ("space/enter", "toggle"),
            ("esc", "back"),
        ],
        Mode::Create(_) => vec![
            ("tab", "next"),
            ("enter", "next/submit"),
            ("F5/ctrl+enter/ctrl+s", "submit"),
            ("esc", "cancel"),
        ],
        Mode::Edit(_) => vec![("enter", "save"), ("esc", "cancel")],
        Mode::Comment(_) => vec![("ctrl+s", "submit"), ("esc", "cancel")],
        Mode::Transition(_) => vec![
            ("j/k", "move"),
            ("enter", "submit"),
            ("esc", "cancel"),
        ],
        Mode::EditTime(_) => vec![
            ("tab", "switch"),
            ("enter", "submit"),
            ("esc", "cancel"),
        ],
        Mode::EditPriority(_) => vec![
            ("j/k", "move"),
            ("enter", "set"),
            ("esc", "cancel"),
        ],
        Mode::StartWorkPrompt(_) => vec![
            ("tab", "switch"),
            ("enter", "submit"),
            ("F5/ctrl+s", "submit"),
            ("esc", "cancel"),
        ],
        Mode::Implementation(_) => vec![
            ("j/k", "scroll"),
            ("s", "save md"),
            ("o", "claude in tmux"),
            ("r", "reload"),
            ("R", "regenerate"),
            ("esc", "back"),
        ],
        Mode::ConfluenceSpaces(_) => vec![
            ("j/k", "move"),
            ("enter", "open space"),
            ("r", "reload"),
            ("esc/q", "back"),
        ],
        Mode::ConfluencePages(form) => if form.search_active {
            vec![
                ("type", "filter"),
                ("enter", "search"),
                ("j/k", "move results"),
                ("esc", "cancel"),
            ]
        } else {
            vec![
                ("j/k", "move"),
                ("/", "search"),
                ("enter", "view page"),
                ("l/→", "drill into children"),
                ("h/←/esc", "back"),
            ]
        },
        Mode::PageView(_) => vec![],  // PageView draws its own footer
        Mode::Tree(form) => {
            let mut hints: Vec<Hint> = vec![
                ("j/k", "move"),
                ("o/Tab", "toggle"),
                ("O/C", "expand/collapse all"),
            ];
            let selected_is_subtask = form
                .visible
                .get(form.selected)
                .and_then(|&i| form.nodes[i].issue_type.as_deref())
                .map(|t| t.eq_ignore_ascii_case("sub-task") || t.eq_ignore_ascii_case("subtask"))
                .unwrap_or(false);
            if !selected_is_subtask {
                hints.push(("c", "create child"));
            }
            hints.extend([
                ("v", "two-col"),
                ("Enter", "detail"),
                ("q", "back"),
            ]);
            hints
        }
        Mode::AssignPicker(_) => vec![
            ("type", "search"),
            ("↑/↓", "move"),
            ("enter", "select"),
            ("esc", "cancel"),
        ],
        Mode::ArchiveConfirm(_) => vec![
            ("y/enter", "confirm"),
            ("n/esc", "cancel"),
        ],
    }
}

/// Greedy whitespace-aware wrap for long error/status messages in modals.
fn wrap_line(s: &str, w: usize) -> Vec<String> {
    if w == 0 || s.len() <= w { return vec![s.to_string()]; }
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        if cur.is_empty() {
            cur.push_str(word);
        } else if cur.len() + 1 + word.len() <= w {
            cur.push(' ');
            cur.push_str(word);
        } else {
            out.push(std::mem::take(&mut cur));
            cur.push_str(word);
        }
        // Hard-break very long words.
        while cur.len() > w {
            let split: String = cur.drain(..w).collect();
            out.push(split);
        }
    }
    if !cur.is_empty() { out.push(cur); }
    out
}

fn draw_archive_confirm(f: &mut Frame, app: &App) {
    use ratatui::layout::{Alignment, Constraint, Direction, Layout};
    let Mode::ArchiveConfirm(form) = &app.mode else { return };
    let total = f.area();
    let has_error = form.error.is_some();
    let height = if has_error { 22u16 } else { 9u16 }.min(total.height.saturating_sub(2));
    let width = (total.width * 70 / 100).max(60).min(total.width.saturating_sub(2));
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length((total.height.saturating_sub(height)) / 2),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(total);
    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length((total.width.saturating_sub(width)) / 2),
            Constraint::Length(width),
            Constraint::Min(0),
        ])
        .split(v[1]);
    let area = h[1];

    f.render_widget(ratatui::widgets::Clear, area);
    let bg = if has_error { Color::Rgb(40, 16, 16) } else { Color::Rgb(20, 28, 32) };
    let border = if has_error { Color::Red } else { Color::Yellow };
    f.render_widget(
        Block::default().style(Style::default().bg(bg)).borders(Borders::NONE),
        area,
    );
    let title = if has_error { " archive failed " } else { " archive ticket? " };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Line::from(vec![
            Span::styled(title, Style::default().fg(border).add_modifier(Modifier::BOLD)),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(form.key.clone(), Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(form.summary.clone(), Style::default()),
        ]),
        Line::from(""),
    ];
    if let Some(err) = &form.error {
        // Hard-wrap on whitespace to the inner width so commas-in-list don't get cut.
        let wrap_w = inner.width.saturating_sub(4) as usize;
        for ln in err.lines() {
            for chunk in wrap_line(ln, wrap_w.max(20)) {
                lines.push(Line::from(Span::styled(
                    format!("  {}", chunk),
                    Style::default().fg(Color::Red),
                )));
            }
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("[ Enter / Esc ]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw("  dismiss"),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "  Will transition the ticket to Won't Do / Cancelled / Closed / Done.",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("[ y / Enter ]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw("  archive    "),
            Span::styled("[ n / Esc ]", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::raw("  cancel"),
        ]));
    }
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Left), inner);
}

fn draw_assign_picker(f: &mut Frame, app: &App) {
    use ratatui::layout::{Constraint, Direction, Layout};
    let Mode::AssignPicker(form) = &app.mode else { return };

    // Center a popup ~60% wide, ~16 rows tall.
    let total = f.area();
    let height = 16u16.min(total.height.saturating_sub(2));
    let width = (total.width * 60 / 100).max(50).min(total.width.saturating_sub(2));
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length((total.height.saturating_sub(height)) / 2),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(total);
    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length((total.width.saturating_sub(width)) / 2),
            Constraint::Length(width),
            Constraint::Min(0),
        ])
        .split(v[1]);
    let area = h[1];

    f.render_widget(ratatui::widgets::Clear, area);
    f.render_widget(
        Block::default()
            .style(Style::default().bg(Color::Rgb(20, 20, 28)))
            .borders(Borders::NONE),
        area,
    );
    let title_text = match form.purpose {
        AssignPurpose::Assignee => format!(" assign {} ", form.key),
        AssignPurpose::Reviewer => format!(" set reviewer on {} ", form.key),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Line::from(vec![
            Span::styled(title_text, Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Layout: query line, hint line, results.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    // Query
    let query_line = Line::from(vec![
        Span::styled("query: ", Style::default().fg(Color::DarkGray)),
        Span::styled(form.query.clone(), Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
        Span::styled("█", Style::default().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(query_line), chunks[0]);

    let hint = match form.purpose {
        AssignPurpose::Assignee => "type ≥ 2 chars · empty + enter = me",
        AssignPurpose::Reviewer => "type ≥ 2 chars · enter to set",
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray)))),
        chunks[1],
    );

    // Results
    let result_lines: Vec<Line> = if form.results.is_empty() {
        vec![Line::from(Span::styled(
            "  (no results)",
            Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
        ))]
    } else {
        form.results
            .iter()
            .enumerate()
            .map(|(i, (name, _id))| {
                let style = if i == form.selected {
                    Style::default().bg(Color::Rgb(60, 60, 80)).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let prefix = if i == form.selected { "▶ " } else { "  " };
                Line::from(vec![
                    Span::styled(prefix.to_string(), Style::default().fg(Color::DarkGray)),
                    Span::styled(name.clone(), style),
                ])
            })
            .collect()
    };
    f.render_widget(Paragraph::new(result_lines), chunks[2]);

    // Error
    if let Some(err) = &form.error {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                err.clone(),
                Style::default().fg(Color::Red),
            ))),
            chunks[3],
        );
    }
}

fn draw_help_overlay(f: &mut Frame, app: &App) {
    use ratatui::layout::{Alignment, Constraint, Direction, Layout};
    let hints = mode_hints(app);
    let total = f.area();
    // Center a popup ~70% wide, autosize height to row count + 4 lines of chrome.
    let rows = (hints.len() as u16).max(1) + 4;
    let height = rows.min(total.height.saturating_sub(2));
    let width = (total.width * 70 / 100).max(40).min(total.width.saturating_sub(2));
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length((total.height.saturating_sub(height)) / 2),
            Constraint::Length(height),
            Constraint::Min(0),
        ])
        .split(total);
    let h = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Length((total.width.saturating_sub(width)) / 2),
            Constraint::Length(width),
            Constraint::Min(0),
        ])
        .split(v[1]);
    let area = h[1];

    // Clear area first so content underneath doesn't bleed through.
    f.render_widget(ratatui::widgets::Clear, area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Line::from(vec![
            Span::styled(" help ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled("(esc/q/? to close)", Style::default().fg(Color::DarkGray)),
        ]));
    let inner = block.inner(area);
    f.render_widget(
        Block::default()
            .style(Style::default().bg(Color::Rgb(20, 20, 28)))
            .borders(Borders::NONE),
        area,
    );
    f.render_widget(block, area);

    let key_style = Style::default().fg(Color::Green).add_modifier(Modifier::BOLD);
    let exp_style = Style::default();
    let max_key_w = hints.iter().map(|(k, _)| k.len()).max().unwrap_or(1);
    let lines: Vec<Line> = hints
        .iter()
        .map(|(k, v)| {
            let pad = " ".repeat(max_key_w.saturating_sub(k.len()));
            Line::from(vec![
                Span::raw("  "),
                Span::styled(k.to_string(), key_style),
                Span::raw(pad),
                Span::raw("  "),
                Span::styled(v.to_string(), exp_style),
            ])
        })
        .collect();
    let p = Paragraph::new(lines).alignment(Alignment::Left);
    f.render_widget(p, inner);
}

fn render_hints(hints: &[Hint]) -> Line<'static> {
    let key_style = Style::default().fg(Color::Green).add_modifier(Modifier::BOLD);
    let sep_style = Style::default().fg(Color::DarkGray);
    let exp_style = Style::default().fg(Color::DarkGray);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(hints.len() * 4);
    for (i, (key, explanation)) in hints.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ", sep_style));
        }
        spans.push(Span::styled(key.to_string(), key_style));
        spans.push(Span::styled(":".to_string(), sep_style));
        spans.push(Span::styled(explanation.to_string(), exp_style));
    }
    Line::from(spans)
}

fn field_line<'a>(label: &'a str, value: &'a str, active: bool) -> Line<'a> {
    let style = if active {
        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let cursor = if active { "▏" } else { "" };
    Line::from(vec![
        Span::styled(label, Style::default().fg(Color::DarkGray)),
        Span::styled(value, style),
        Span::styled(cursor, Style::default().fg(Color::Cyan)),
    ])
}

/// Single-width Unicode glyph + style mimicking Jira's web-UI issue-type icons:
///   Epic ⚡ (magenta), Story ✦ (green), Task ✓ (cyan), Sub-task ↳ (dim cyan),
///   Bug ● (red), unknown · (dark gray).
fn issue_type_glyph(typ: Option<&str>) -> (&'static str, Style) {
    let t = typ.unwrap_or("").to_ascii_lowercase();
    match t.as_str() {
        "epic" => ("⚡", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        "story" => ("✦", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
        "task" => ("✓", Style::default().fg(Color::Cyan)),
        "sub-task" | "subtask" => ("↳", Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM)),
        "bug" => ("●", Style::default().fg(Color::Red)),
        "improvement" => ("↑", Style::default().fg(Color::Blue)),
        "spike" => ("◇", Style::default().fg(Color::Yellow)),
        _ => ("·", Style::default().fg(Color::DarkGray)),
    }
}

/// Render the priority as a fixed-width tag with color matching its rank.
fn priority_span<'a>(p: Option<&'a str>) -> Span<'a> {
    let label = format!("{:<10}", p.unwrap_or("—"));
    let color = match priority_rank(p) {
        1 => Color::Red,
        2 => Color::LightRed,
        3 => Color::Yellow,
        4 => Color::Cyan,
        5 => Color::DarkGray,
        _ => Color::DarkGray,
    };
    Span::styled(label, Style::default().fg(color))
}

/// One-line dim breadcrumb shown above a subtask/child ticket — empty if no parent info.
/// Format: "  ↪ Grandparent › Parent" with whichever pieces are available.
fn breadcrumb_text(t: &jui_core::ticket::Ticket) -> Option<String> {
    if t.parent_key.is_none() && t.grandparent_key.is_none() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = &t.grandparent_summary {
        parts.push(s.clone());
    } else if let Some(k) = &t.grandparent_key {
        parts.push(k.clone());
    }
    if let Some(s) = &t.parent_summary {
        parts.push(s.clone());
    } else if let Some(k) = &t.parent_key {
        parts.push(k.clone());
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join(" › "))
}

fn breadcrumb_line_from_text(text: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("  ↪ {}", text),
        Style::default().fg(Color::DarkGray),
    ))
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n { s.to_string() } else { format!("{}…", &s[..n.saturating_sub(1)]) }
}

fn draw_confluence_spaces(f: &mut Frame, area: Rect, app: &App) {
    let Mode::ConfluenceSpaces(form) = &app.mode else { return };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" confluence spaces ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    if form.loading {
        let p = Paragraph::new("loading spaces…").style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, inner);
        return;
    }
    if let Some(err) = &form.error {
        let p = Paragraph::new(format!("error: {err}"))
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: true });
        f.render_widget(p, inner);
        return;
    }
    if form.spaces.is_empty() {
        let p = Paragraph::new(
            Span::styled("no spaces found", Style::default().fg(Color::DarkGray))
        );
        f.render_widget(p, inner);
        return;
    }
    let items: Vec<ListItem> = form
        .spaces
        .iter()
        .map(|s| {
            let key_span = Span::styled(
                format!(" {:<12} ", s.key),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            );
            let name_span = Span::raw(s.name.clone());
            let mut spans = vec![key_span, name_span];
            if !s.description.is_empty() {
                spans.push(Span::styled(
                    format!("  — {}", truncate(&s.description, 60)),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(form.selected.min(form.spaces.len() - 1)));
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, inner, &mut state);
}

fn draw_confluence_pages(f: &mut Frame, area: Rect, app: &App) {
    let Mode::ConfluencePages(form) = &app.mode else { return };
    let crumb_display = if form.breadcrumb.is_empty() {
        format!(" {} — pages ", form.space_name)
    } else {
        let path = form
            .breadcrumb
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>()
            .join(" › ");
        format!(" {} › {} ", form.space_name, path)
    };
    let block = Block::default().borders(Borders::ALL).title(crumb_display);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if form.loading {
        let p = Paragraph::new("loading pages…").style(Style::default().fg(Color::DarkGray));
        f.render_widget(p, inner);
        return;
    }
    if let Some(err) = &form.error {
        let p = Paragraph::new(format!("error: {err}"))
            .style(Style::default().fg(Color::Red))
            .wrap(Wrap { trim: true });
        f.render_widget(p, inner);
        return;
    }

    if form.search_active {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(1)])
            .split(inner);

        // Search bar.
        let search_line = Line::from(vec![
            Span::styled("/", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {}▏", form.search_query),
                Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  (enter to search, esc to cancel)",
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        f.render_widget(Paragraph::new(search_line), chunks[0]);

        // Results area.
        if form.search_loading {
            f.render_widget(
                Paragraph::new("searching…").style(Style::default().fg(Color::DarkGray)),
                chunks[1],
            );
        } else if let Some(err) = &form.search_error {
            f.render_widget(
                Paragraph::new(format!("error: {err}")).style(Style::default().fg(Color::Red)),
                chunks[1],
            );
        } else if form.search_results.is_empty() && !form.search_query.is_empty() {
            f.render_widget(
                Paragraph::new(Span::styled(
                    "no results — press enter to search",
                    Style::default().fg(Color::DarkGray),
                )),
                chunks[1],
            );
        } else {
            let items: Vec<ListItem> = form
                .search_results
                .iter()
                .map(|p| {
                    let marker = if p.has_children {
                        Span::styled(" ▸", Style::default().fg(Color::DarkGray))
                    } else {
                        Span::raw("  ")
                    };
                    ListItem::new(Line::from(vec![
                        Span::raw(" "),
                        Span::raw(p.title.clone()),
                        marker,
                    ]))
                })
                .collect();
            let sel = if items.is_empty() {
                None
            } else {
                Some(form.search_selected.min(items.len() - 1))
            };
            let mut state = ListState::default();
            state.select(sel);
            let list = List::new(items)
                .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, chunks[1], &mut state);
        }
        return;
    }

    // Normal page list.
    if form.pages.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled("no pages", Style::default().fg(Color::DarkGray))),
            inner,
        );
        return;
    }
    let items: Vec<ListItem> = form
        .pages
        .iter()
        .map(|p| {
            let child_marker = if p.has_children {
                Span::styled(" ▸", Style::default().fg(Color::DarkGray))
            } else {
                Span::raw("  ")
            };
            ListItem::new(Line::from(vec![
                Span::raw(" "),
                Span::raw(p.title.clone()),
                child_marker,
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(form.selected.min(form.pages.len() - 1)));
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, inner, &mut state);
}

fn draw_page_view(f: &mut Frame, area: Rect, app: &mut App) {
    use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
    use ratatui_image::StatefulImage;
    let Mode::PageView(form) = &mut app.mode else { return };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);

    // Header
    let hdr = Paragraph::new(Line::from(vec![
        Span::styled(" ", Style::default()),
        Span::styled(form.title.clone(), Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::styled(
            format!("  [{}/{}]", form.scroll + 1, form.lines.len().max(1)),
            Style::default().fg(Color::DarkGray),
        ),
    ]));
    f.render_widget(hdr, chunks[0]);

    // Content
    let content_area = chunks[1];
    let viewport_h = content_area.height as usize;
    let max_scroll = form.lines.len().saturating_sub(viewport_h);
    let scroll = form.scroll.min(max_scroll);
    let end = (scroll + viewport_h).min(form.lines.len());

    let visible_count = end - scroll;
    let visible: Vec<(usize, PageLine)> = (scroll..end)
        .map(|i| (i, match &form.lines[i] {
            PageLine::Spans(s) => PageLine::Spans(s.clone()),
            PageLine::Blank => PageLine::Blank,
            PageLine::Image { id, row, height } => PageLine::Image { id: *id, row: *row, height: *height },
        }))
        .collect();

    for row_idx in 0..visible_count {
        let (global_idx, line) = &visible[row_idx];
        let y = content_area.y + row_idx as u16;
        if y >= content_area.y + content_area.height { break; }
        let line_area = Rect::new(content_area.x, y, content_area.width.saturating_sub(1), 1);
        let is_cursor = form.search_matches.get(form.search_cursor) == Some(global_idx);
        let is_match = !is_cursor && form.search_matches.binary_search(global_idx).is_ok();

        match line {
            PageLine::Spans(spans) => {
                let styled: Vec<Span<'static>> = if is_cursor {
                    spans.iter().map(|s| Span::styled(s.content.clone(), s.style.bg(Color::Rgb(80, 60, 0)))).collect()
                } else if is_match {
                    spans.iter().map(|s| Span::styled(s.content.clone(), s.style.bg(Color::Rgb(40, 40, 40)))).collect()
                } else {
                    spans.clone()
                };
                f.render_widget(Paragraph::new(Line::from(styled)), line_area);
            }
            PageLine::Image { id, row, height } if *row == 0 => {
                let img_height = (*height).min(content_area.height.saturating_sub(row_idx as u16));
                if img_height > 0 {
                    let img_area = Rect::new(
                        content_area.x,
                        y,
                        content_area.width.saturating_sub(1),
                        img_height,
                    );
                    if let Some(pi) = form.images.get_mut(*id) {
                        f.render_stateful_widget(StatefulImage::default(), img_area, &mut pi.proto);
                    }
                }
            }
            _ => {}
        }
    }

    // Scrollbar
    if form.lines.len() > viewport_h {
        let sb_area = Rect::new(
            content_area.x + content_area.width.saturating_sub(1),
            content_area.y, 1, content_area.height,
        );
        let mut sb_state = ScrollbarState::new(max_scroll).position(scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight).begin_symbol(None).end_symbol(None),
            sb_area, &mut sb_state,
        );
    }

    // Footer
    let footer_area = chunks[2];
    let block = Block::default().borders(Borders::TOP);
    let inner = block.inner(footer_area);
    f.render_widget(block, footer_area);

    if form.search_active {
        let match_info = if form.search_matches.is_empty() {
            if form.search_query.is_empty() { String::new() } else { "no matches".to_string() }
        } else {
            format!("{}/{}", form.search_cursor + 1, form.search_matches.len())
        };
        let search_line = Line::from(vec![
            Span::styled("/ ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
            Span::styled(form.search_query.clone(), Style::default()),
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(
                if match_info.is_empty() { String::new() } else { format!("  {}", match_info) },
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        let hint_line = Line::from(vec![
            Span::styled("n/N", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(" next/prev  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Esc", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(" close", Style::default().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(vec![search_line, hint_line]), inner);
    } else {
        let pct = if form.lines.is_empty() { 100 } else { (scroll * 100 / form.lines.len()).min(100) };
        let hints = Line::from(vec![
            Span::styled("j/k", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(" scroll  ", Style::default().fg(Color::DarkGray)),
            Span::styled("d/u", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(" ½pg  ", Style::default().fg(Color::DarkGray)),
            Span::styled("/", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(" search  ", Style::default().fg(Color::DarkGray)),
            Span::styled("e", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(" edit  ", Style::default().fg(Color::DarkGray)),
            Span::styled("S", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(" sync  ", Style::default().fg(Color::DarkGray)),
            Span::styled("q", Style::default().fg(Color::Green).add_modifier(Modifier::BOLD)),
            Span::styled(" back  ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("{}%", pct), Style::default().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(hints), inner);
    }
}

fn type_color(issue_type: Option<&str>) -> Color {
    match issue_type.unwrap_or("") {
        t if t.eq_ignore_ascii_case("epic") => Color::Magenta,
        t if t.eq_ignore_ascii_case("story") => Color::Green,
        t if t.eq_ignore_ascii_case("task") => Color::Blue,
        t if t.eq_ignore_ascii_case("bug") => Color::Red,
        t if t.eq_ignore_ascii_case("sub-task") || t.eq_ignore_ascii_case("subtask") => Color::DarkGray,
        _ => Color::White,
    }
}

fn type_glyph(issue_type: Option<&str>) -> &'static str {
    match issue_type.unwrap_or("") {
        t if t.eq_ignore_ascii_case("epic") => "⚡",
        t if t.eq_ignore_ascii_case("story") => "✦",
        t if t.eq_ignore_ascii_case("task") => "☑",
        t if t.eq_ignore_ascii_case("bug") => "✗",
        t if t.eq_ignore_ascii_case("sub-task") | t.eq_ignore_ascii_case("subtask") => "↳",
        t if t.eq_ignore_ascii_case("improvement") => "▲",
        t if t.eq_ignore_ascii_case("spike") => "✱",
        _ => "○",
    }
}

/// Short fixed-width type label so columns line up across the tree. 5 chars padded.
fn type_label(issue_type: Option<&str>) -> &'static str {
    match issue_type.unwrap_or("") {
        t if t.eq_ignore_ascii_case("epic") => "Epic ",
        t if t.eq_ignore_ascii_case("story") => "Story",
        t if t.eq_ignore_ascii_case("task") => "Task ",
        t if t.eq_ignore_ascii_case("bug") => "Bug  ",
        t if t.eq_ignore_ascii_case("sub-task") | t.eq_ignore_ascii_case("subtask") => "Sub  ",
        t if t.eq_ignore_ascii_case("improvement") => "Impr ",
        t if t.eq_ignore_ascii_case("spike") => "Spike",
        _ => "?    ",
    }
}

fn tree_node_line(node: &TreeNode, selected: bool) -> Line<'static> {
    let indent = "  ".repeat(node.depth as usize);
    let arrow = if !node.children.is_empty() {
        if node.expanded { "▼ " } else { "▶ " }
    } else {
        "  "
    };
    let glyph = type_glyph(node.issue_type.as_deref());
    let key_style = Style::default()
        .fg(type_color(node.issue_type.as_deref()))
        .add_modifier(Modifier::BOLD);
    let summary_style = if node.is_mine {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC)
    };
    let bg = if selected {
        Style::default().bg(Color::Rgb(60, 60, 80))
    } else {
        Style::default()
    };
    let summary = if node.summary.is_empty() { "—".to_string() } else { node.summary.clone() };
    let type_color_v = type_color(node.issue_type.as_deref());
    let label = type_label(node.issue_type.as_deref());
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(format!("{indent}{arrow}"), Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{glyph} "), Style::default().fg(type_color_v)),
        Span::styled(label.to_string(), Style::default().fg(type_color_v).add_modifier(Modifier::BOLD)),
        Span::styled("  ", Style::default()),
    ];
    // Role badge (only on leaves the user actually owns; ancestors get blanks
    // so columns line up).
    let badge_width = "[X] ".len();
    if let Some(role) = node.role {
        let (badge, style) = role_badge(role);
        spans.push(Span::styled(badge.to_string(), style));
        spans.push(Span::raw(" "));
    } else {
        spans.push(Span::raw(" ".repeat(badge_width)));
    }
    spans.push(Span::styled(node.key.clone(), key_style));
    spans.push(Span::styled("  ", Style::default()));
    spans.push(Span::styled(summary, summary_style));
    spans.push(Span::styled(format!("  [{}]", node.status), Style::default().fg(Color::DarkGray)));
    Line::from(spans).style(bg)
}

fn draw_tree(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Tree(form) = &app.mode else { return };
    if form.two_column {
        draw_tree_two_column(f, area, form);
    } else {
        draw_tree_single(f, area, form);
    }
}

fn draw_tree_single(f: &mut Frame, area: Rect, form: &TreeForm) {
    let inner = Block::default()
        .borders(Borders::ALL)
        .title(" tickets — tree (T) ")
        .inner(area);
    f.render_widget(
        Block::default().borders(Borders::ALL).title(" tickets — tree (T) "),
        area,
    );
    if form.visible.is_empty() {
        f.render_widget(
            Paragraph::new("no tickets").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    let viewport_h = inner.height as usize;
    let scroll = form.selected.saturating_sub(viewport_h.saturating_sub(1) / 2);
    let scroll = scroll.min(form.visible.len().saturating_sub(viewport_h).max(0));
    let end = (scroll + viewport_h).min(form.visible.len());
    let lines: Vec<Line> = (scroll..end)
        .map(|i| {
            let node_idx = form.visible[i];
            tree_node_line(&form.nodes[node_idx], i == form.selected)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_tree_two_column(f: &mut Frame, area: Rect, form: &TreeForm) {
    use ratatui::layout::{Constraint, Direction, Layout};
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(35), Constraint::Min(0)])
        .split(area);

    // Left: roots only.
    let left_block = Block::default().borders(Borders::ALL).title(" roots ");
    let left_inner = left_block.inner(chunks[0]);
    f.render_widget(left_block, chunks[0]);

    // Find which root contains the current selection.
    let mut selected_root_idx: usize = 0;
    if let Some(&sel_node) = form.visible.get(form.selected) {
        let mut cur = sel_node;
        loop {
            if let Some(pos) = form.roots.iter().position(|&r| r == cur) {
                selected_root_idx = pos;
                break;
            }
            // Walk up via parent_key (could refactor by storing parent index, but cheap).
            let parent_key = form.nodes[cur].parent_key.clone();
            let Some(pk) = parent_key else { break };
            let Some(p_idx) = form.nodes.iter().position(|n| n.key == pk) else { break };
            cur = p_idx;
        }
    }
    let root_lines: Vec<Line> = form
        .roots
        .iter()
        .enumerate()
        .map(|(i, &r)| {
            let n = &form.nodes[r];
            let style = if i == selected_root_idx {
                Style::default().bg(Color::Rgb(60, 60, 80)).add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(format!("{} ", type_glyph(n.issue_type.as_deref())),
                    Style::default().fg(type_color(n.issue_type.as_deref()))),
                Span::styled(n.key.clone(), Style::default().fg(type_color(n.issue_type.as_deref())).add_modifier(Modifier::BOLD)),
                Span::raw("  "),
                Span::styled(n.summary.clone(), Style::default()),
            ])
            .style(style)
        })
        .collect();
    f.render_widget(Paragraph::new(root_lines), left_inner);

    // Right: subtree of selected root.
    let right_block = Block::default().borders(Borders::ALL).title(" subtree ");
    let right_inner = right_block.inner(chunks[1]);
    f.render_widget(right_block, chunks[1]);

    let Some(&root_idx) = form.roots.get(selected_root_idx) else { return };
    let mut subtree_visible: Vec<usize> = Vec::new();
    push_subtree(&form.nodes, root_idx, &mut subtree_visible);
    let viewport_h = right_inner.height as usize;
    // selected position within subtree_visible (if applicable)
    let sel_in_sub = subtree_visible
        .iter()
        .position(|&n| Some(&n) == form.visible.get(form.selected))
        .unwrap_or(0);
    let scroll = sel_in_sub.saturating_sub(viewport_h.saturating_sub(1) / 2);
    let scroll = scroll.min(subtree_visible.len().saturating_sub(viewport_h).max(0));
    let end = (scroll + viewport_h).min(subtree_visible.len());
    let lines: Vec<Line> = (scroll..end)
        .map(|i| {
            let node_idx = subtree_visible[i];
            let is_sel = Some(&node_idx) == form.visible.get(form.selected);
            tree_node_line(&form.nodes[node_idx], is_sel)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), right_inner);
}

fn push_subtree(nodes: &[TreeNode], idx: usize, out: &mut Vec<usize>) {
    let mut visited: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut stack: Vec<usize> = vec![idx];
    while let Some(i) = stack.pop() {
        if !visited.insert(i) { continue; }
        out.push(i);
        if nodes[i].expanded {
            // push children in reverse so visual order is preserved when popping
            for &c in nodes[i].children.iter().rev() {
                if !visited.contains(&c) { stack.push(c); }
            }
        }
    }
}
