use crate::app::{
    App, AssignPurpose, DetailFocus, DetailLinkedProject, MentionRole, Mode, PageLine,
    PendingDelete, PrUserState, TicketOptionAction, TreeForm, TreeNode,
};
use jui_core::ticket::{fmt_date, fmt_seconds, parse_reply, priority_rank, Comment};
use ratatui::layout::Alignment;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
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
        Mode::DevQaPrompt(_) => draw_devqa_prompt(f, chunks[1], app),
        Mode::DevQaResolveConfirm(_) => draw_devqa_resolve_confirm(f, chunks[1], app),
        Mode::DevQaCleanupConfirm(_) => draw_devqa_cleanup_confirm(f, chunks[1], app),
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
        Mode::TicketOptions(_) => {
            draw_detail(f, chunks[1], app);
            draw_ticket_options(f, app);
        }
        Mode::PrCreate(_) => {
            draw_detail(f, chunks[1], app);
            draw_pr_create(f, app);
        }
        Mode::ActiveStatusConfig(_) => draw_active_status_config(f, chunks[1], app),
        Mode::Settings(_) => draw_settings(f, chunks[1], app),
        Mode::Rules(_) => draw_rules(f, chunks[1], app),
        Mode::RuleEdit(_) => draw_rule_edit(f, chunks[1], app),
        Mode::RuleLog(_) => draw_rule_log(f, chunks[1], app),
        Mode::Home(_) => draw_home(f, chunks[1], app),
        Mode::PrCommentReply(_) => {
            // Render Detail underneath so the PR-comments context stays
            // visible behind the modal.
            draw_detail(f, chunks[1], app);
            draw_pr_comment_reply(f, app);
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
        Mode::DevQaPrompt(_) => "begin dev qa",
        Mode::DevQaResolveConfirm(_) => "resolve dev qa",
        Mode::DevQaCleanupConfirm(_) => "remove dev qa worktree",
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
            AssignPurpose::DevQa => "dev qa",
        },
        Mode::ArchiveConfirm(_) => "archive?",
        Mode::TicketOptions(_) => "options",
        Mode::PrCreate(_) => "pr",
        Mode::ActiveStatusConfig(_) => "workflow",
        Mode::Settings(_) => "settings",
        Mode::Rules(_) => "rules",
        Mode::RuleEdit(_) => "rule-edit",
        Mode::RuleLog(_) => "rule-log",
        Mode::Home(_) => "home",
        Mode::PrCommentReply(_) => "pr reply",
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
    let cyan_bold = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let prefix = format!(" jui — {}  |  ", mode);
    // Heuristic: surface error-shaped status messages in bold red so silent
    // failures (e.g. dirty-tree start-work) stop hiding in the noise.
    let s_lc = app.status.to_ascii_lowercase();
    let is_err = s_lc.contains(" err:")
        || s_lc.starts_with("err:")
        || s_lc.contains("failed")
        || s_lc.contains("error:")
        || s_lc.contains("unexpected");
    let status_span = if is_err {
        Span::styled(
            app.status.clone(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(app.status.clone(), cyan_bold)
    };
    let line = Line::from(vec![Span::styled(prefix, cyan_bold), status_span]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_list(f: &mut Frame, area: Rect, app: &App) {
    use crate::app::ListFocus;
    use ratatui::layout::{Constraint, Direction, Layout};
    // Split into Active (top) + Mentioned (bottom). When focused, the Mentioned
    // section grabs ~70% of the inner area so the user can scroll long lists
    // comfortably. Otherwise it stays compact (row-count based, capped at 40%).
    let mentioned_rows = (app.reviewing_tickets.len()
        + app.github_tickets.len()
        + app.mentioned_tickets.len()) as u16;
    let focused = app.list_focus == ListFocus::Mentioned;
    let mentioned_h = if focused {
        ((area.height * 70) / 100).max(8)
    } else if mentioned_rows == 0 {
        4 // "(none)" stub + border
    } else {
        (mentioned_rows + 2).clamp(5, (area.height * 40 / 100).max(5))
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(8), Constraint::Length(mentioned_h)])
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
    let rows = app.active_search_rows();
    let total_active = rows.len();
    let items: Vec<ListItem> = rows
        .iter()
        .filter_map(|(row_pos, i)| {
            let t = app.tickets.get(*i)?;
            let depth = app.active_row_depths.get(*row_pos).copied().unwrap_or(0);
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
                spans.push(Span::styled(
                    format!("{indent}└─ "),
                    Style::default().fg(Color::DarkGray),
                ));
            } else if child_count > 0 {
                let glyph = if expanded { "▾ " } else { "▸ " };
                let style = if expanded {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
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
                format!(
                    "{:<width$} ",
                    truncate(&t.status, status_width),
                    width = status_width
                ),
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
    state.select(if total_active == 0 || !focused {
        None
    } else {
        Some(app.list_selected.min(total_active - 1))
    });
    let search_label = if app.ticket_search_active || !app.ticket_search_query.is_empty() {
        format!(" · /{}", app.ticket_search_query)
    } else {
        String::new()
    };
    let title = format!(
        " Jira Assigned — {}{} · sort: {} {}",
        total_active,
        search_label,
        app.sort_mode.label(),
        if app.inactive_idxs.is_empty() {
            String::new()
        } else {
            format!(" ({} archived — 'a')", app.inactive_idxs.len())
        }
    );
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(focus_border(focused))
                .title(title),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(if focused { "▶ " } else { "  " });
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_list_mentioned(f: &mut Frame, area: Rect, app: &App) {
    use crate::app::ListFocus;
    let focused = app.list_focus == ListFocus::Mentioned;
    let combined = app.combined_mentions();
    let total = combined.len();
    let r = app.reviewing_tickets.len();
    let g = app.github_tickets.len();
    let m = app.mentioned_tickets.len();
    let title = format!(
        " reviewing + github + mentioned — {r} jira-R · {g} gh-R · {m} @ (Shift+Tab to focus) "
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
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )),
            inner,
        );
        return;
    }

    // Auto-size the status column to the longest status across visible rows,
    // capped so it doesn't crowd the summary on a narrow terminal.
    let status_w: usize = combined
        .iter()
        .map(|(_, t)| t.status.chars().count())
        .max()
        .unwrap_or(14)
        .clamp(14, 28);

    let items: Vec<ListItem> = combined
        .iter()
        .map(|(role, t)| {
            let (badge, badge_style) = role_badge(*role);
            let (glyph, glyph_style) = issue_type_glyph(t.issue_type.as_deref());
            let pr_label = devqa_or_pr_label(*role, &t.status, Some(app.pr_state(&t.key)));
            let mut spans: Vec<Span> = Vec::with_capacity(10);
            spans.push(Span::raw(" "));
            // Lead with the user's review state — most prominent column.
            if let Some((text, st)) = pr_label {
                spans.push(Span::styled(text.to_string(), st));
            } else {
                spans.push(Span::raw("       "));
            }
            spans.push(Span::styled(format!("{badge} "), badge_style));
            spans.push(Span::styled(format!("{glyph} "), glyph_style));
            spans.push(Span::styled(
                format!("{:<12} ", t.key),
                Style::default().fg(Color::Yellow),
            ));
            // Status column auto-sizes to the longest visible status, capped
            // at 28 chars so workflow names like "Firmware Dev QA In Progress"
            // fit and the priority + summary aren't pushed under it.
            spans.push(Span::styled(
                format!(
                    "{:<width$} ",
                    truncate(&t.status, status_w),
                    width = status_w
                ),
                Style::default().fg(Color::Green),
            ));
            spans.push(priority_span(t.priority.as_deref()));
            spans.push(Span::raw(" "));
            spans.push(Span::raw(t.summary.clone()));
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(if focused && total > 0 {
        Some(app.mentioned_selected.min(total - 1))
    } else {
        None
    });
    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(if focused { "▶ " } else { "  " });
    f.render_stateful_widget(list, area, &mut state);
}

/// Badge for the list/tree review-state column. A "STARTED" DevQA badge takes
/// precedence when the ticket's Jira status is a "Dev QA In Progress" state
/// (reviewer/github rows only — those are the ones not assigned to you); else it
/// falls back to the user's local PR review state (AWAIT/REVIEW/DONE). `None`
/// when neither applies. Padded to the same 7 cols as `pr_state_label`.
fn devqa_or_pr_label(
    role: MentionRole,
    status: &str,
    pr_state: Option<PrUserState>,
) -> Option<(&'static str, Style)> {
    if matches!(role, MentionRole::Github | MentionRole::Reviewer)
        && status.to_ascii_lowercase().contains("dev qa in progress")
    {
        return Some((
            "STARTED",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    }
    pr_state_label(role, pr_state?)
}

/// 9-char fixed-width "AWAIT/REVIEW/DONE" label for the user's PR review state.
/// Padded so columns line up across rows. Returns `None` when the role isn't
/// a PR-bearing one (no point labelling pure Jira reviewer/@-mention rows).
fn pr_state_label(role: MentionRole, state: PrUserState) -> Option<(&'static str, Style)> {
    if !matches!(role, MentionRole::Github | MentionRole::Reviewer) {
        return None;
    }
    Some(match state {
        PrUserState::Awaiting => (
            "AWAIT  ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        PrUserState::Reviewing => (
            "REVIEW ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        PrUserState::Completed => (
            "DONE   ",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::DIM),
        ),
    })
}

/// Badge text + style for a role. Color is the source-of-truth identifier:
///   purple → Jira-side badge ([A] assigned, [R] reviewer)
///   blue   → GitHub-side badge ([R] PR reviewer, [@] mentioned)
/// `[R]` deliberately doubles up: Jira-reviewer and GitHub-reviewer share the
/// glyph; the user reads color to tell them apart.
fn role_badge(role: MentionRole) -> (&'static str, Style) {
    let purple = Color::Rgb(170, 130, 255); // Jira side
    let blue = Color::Rgb(80, 160, 255); // GitHub side
    match role {
        MentionRole::Assigned => (
            "[A]",
            Style::default().fg(purple).add_modifier(Modifier::BOLD),
        ),
        MentionRole::Reviewer => (
            "[R]",
            Style::default().fg(purple).add_modifier(Modifier::BOLD),
        ),
        MentionRole::Github => (
            "[R]",
            Style::default().fg(blue).add_modifier(Modifier::BOLD),
        ),
        MentionRole::Mentioned => (
            "[@]",
            Style::default().fg(blue).add_modifier(Modifier::BOLD),
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
    state.select(if app.inactive_idxs.is_empty() {
        None
    } else {
        Some(app.archive_selected)
    });
    let title = format!(
        " archive — {} resolved/done/closed · sort: updated ",
        app.inactive_idxs.len()
    );
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
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
        let mut names: Vec<&str> = app
            .kanban_assignee_filter
            .iter()
            .map(|s| s.as_str())
            .collect();
        names.sort();
        format!(
            " kanban: {} — {} tickets ({} extra) · {} columns ",
            names.join(", "),
            total,
            app.kanban_extra.len(),
            n
        )
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
        let strip = Rect {
            x,
            y: inner.y,
            width: strip_w,
            height: inner.height,
        };
        let (status, idxs) = &cols[ci];
        draw_minimized_column(f, strip, status, idxs.len(), ci == app.kanban_col);
    }

    // Draw minimised strips on the right edge.
    for (slot, &ci) in right_min.iter().enumerate() {
        let x = inner.x + left_strips_w + middle_w + slot as u16 * strip_w;
        let strip = Rect {
            x,
            y: inner.y,
            width: strip_w,
            height: inner.height,
        };
        let (status, idxs) = &cols[ci];
        draw_minimized_column(f, strip, status, idxs.len(), ci == app.kanban_col);
    }

    // Draw expanded columns in the middle area with a sliding window.
    if expanded_idxs.is_empty() || middle_w == 0 {
        return;
    }
    let middle_area = Rect {
        x: inner.x + left_strips_w,
        y: inner.y,
        width: middle_w,
        height: inner.height,
    };
    let min_col_w: u16 = 22;
    let max_visible = ((middle_w / min_col_w) as usize).max(1);
    let visible_n = expanded_idxs.len().min(max_visible);
    // Slide window to keep focused expanded column visible.
    let focused_pos = expanded_idxs
        .iter()
        .position(|&i| i == app.kanban_col)
        .unwrap_or(0);
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
        let cell = Rect {
            x: inner.x,
            y: inner.y + row as u16,
            width: 1,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                ch.to_string(),
                Style::default().fg(text_color),
            )),
            cell,
        );
    }

    // Count badge at bottom.
    if count > 0 && inner.height > 0 {
        let count_str = count.to_string();
        let cy = inner.y + inner.height - 1;
        let cell = Rect {
            x: inner.x,
            y: cy,
            width: 1.max(count_str.len() as u16).min(inner.width),
            height: 1,
        };
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
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::BOLD)
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
        let card_area = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: card_h,
        };
        let selected = focused && i == card_sel;
        let Some(t) = app.kanban_ticket(idxs[i]) else {
            continue;
        };
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
    let title = format!(
        " {} {} — expanded (e to collapse) ",
        status.to_uppercase(),
        idxs.len()
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
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
        let card_area = Rect {
            x: inner.x,
            y,
            width: inner.width,
            height: card_h,
        };
        let selected = i == card_sel;
        let Some(t) = app.kanban_ticket(idxs[i]) else {
            continue;
        };
        draw_kanban_card_expanded(f, card_area, t, selected);
    }
}

fn draw_kanban_card_expanded(
    f: &mut Frame,
    area: Rect,
    t: &jui_core::ticket::Ticket,
    selected: bool,
) {
    use jui_core::ticket::fmt_seconds;
    let border_style = if selected {
        Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height == 0 || inner.width == 0 {
        return;
    }
    let w = inner.width as usize;

    // Row 1: key  type  parent summary
    let issue_type = t.issue_type.as_deref().unwrap_or("");
    let parent_budget = w.saturating_sub(t.key.len() + issue_type.len() + 4);
    let parent_text = truncate(
        t.parent_summary.as_deref().unwrap_or_default(),
        parent_budget,
    );
    let key_line = Line::from(vec![
        Span::styled(
            t.key.clone(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(issue_type.to_string(), Style::default().fg(Color::Blue)),
        Span::raw(if issue_type.is_empty() {
            String::new()
        } else {
            "  ".to_string()
        }),
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
        Style::default()
            .fg(Color::LightCyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style);
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
        Span::styled(
            t.key.clone(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
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
    let hash = name
        .bytes()
        .fold(0u32, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u32));
    PALETTE[(hash as usize) % PALETTE.len()]
}

fn draw_kanban_filter(f: &mut Frame, area: Rect, app: &App) {
    let Mode::KanbanFilter(form) = &app.mode else {
        return;
    };

    let popup_w = 56_u16.min(area.width.saturating_sub(4));
    // Extra rows if teams exist or save prompt active.
    let save_rows: u16 = if form.save_name.is_some() { 2 } else { 0 };
    let team_rows: u16 = if form.teams.is_empty() {
        0
    } else {
        form.teams.len() as u16 + 1
    }; // +1 separator
    let popup_h = (4 + team_rows + save_rows + 18).min(area.height.saturating_sub(4));
    let x = area.x + (area.width.saturating_sub(popup_w)) / 2;
    let y = area.y + (area.height.saturating_sub(popup_h)) / 2;
    let popup_area = Rect {
        x,
        y,
        width: popup_w,
        height: popup_h,
    };

    f.render_widget(ratatui::widgets::Clear, popup_area);

    let filter_label = if app.kanban_assignee_filter.is_empty() {
        " filter users ".to_string()
    } else {
        format!(" filter: {} selected ", app.kanban_assignee_filter.len())
    };
    let source_hint = if form.from_cache {
        " ·  ticket assignees only"
    } else {
        ""
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            format!("{}{}", filter_label, source_hint),
            Style::default()
                .fg(if form.from_cache {
                    Color::Yellow
                } else {
                    Color::Cyan
                })
                .add_modifier(Modifier::BOLD),
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
        f.render_widget(
            hdr,
            Rect {
                x: x0,
                y: y_cursor,
                width: w,
                height: 1,
            },
        );
        y_cursor += 1;

        for (i, team) in form.teams.iter().enumerate() {
            if y_cursor >= inner.y.saturating_add(inner.height) {
                break;
            }
            let cursor = form.save_name.is_none() && i == form.selected;
            let member_str = team.members.join(", ");
            let label = truncate(&format!("★ {}  ({})", team.name, member_str), w as usize);
            let style = if cursor {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
                    .bg(Color::DarkGray)
            } else {
                Style::default().fg(Color::Yellow)
            };
            let p = Paragraph::new(Span::styled(label, style));
            f.render_widget(
                p,
                Rect {
                    x: x0,
                    y: y_cursor,
                    width: w,
                    height: 1,
                },
            );
            y_cursor += 1;
        }

        // Separator line.
        if y_cursor >= inner.y.saturating_add(inner.height) {
            return;
        }
        let sep = Paragraph::new(Span::styled(
            "─".repeat(w as usize),
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(
            sep,
            Rect {
                x: x0,
                y: y_cursor,
                width: w,
                height: 1,
            },
        );
        y_cursor += 1;
    }

    // --- Save-name input ---
    if let Some(ref name) = form.save_name {
        if y_cursor >= inner.y.saturating_add(inner.height) {
            return;
        }
        let prompt = Paragraph::new(Line::from(vec![
            Span::styled("  Save as team: ", Style::default().fg(Color::Cyan)),
            Span::styled(
                format!("{}_", name),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        f.render_widget(
            prompt,
            Rect {
                x: x0,
                y: y_cursor,
                width: w,
                height: 1,
            },
        );
        y_cursor += 1;
        if y_cursor >= inner.y.saturating_add(inner.height) {
            return;
        }
        let sep = Paragraph::new(Span::styled(
            "─".repeat(w as usize),
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(
            sep,
            Rect {
                x: x0,
                y: y_cursor,
                width: w,
                height: 1,
            },
        );
        y_cursor += 1;
    }

    // --- Search box ---
    let remaining_h = inner
        .y
        .saturating_add(inner.height)
        .saturating_sub(y_cursor);
    if remaining_h < 2 {
        return;
    }

    let search_area = Rect {
        x: x0,
        y: y_cursor,
        width: w,
        height: 1,
    };
    let list_area = Rect {
        x: x0,
        y: y_cursor + 1,
        width: w,
        height: remaining_h - 1,
    };

    let search_p = Paragraph::new(format!("/ {}_", form.query)).style(
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    );
    f.render_widget(search_p, search_area);

    // Build user list, offset selection by teams count.
    let n_teams = form.teams.len();
    let user_sel = form.selected.saturating_sub(n_teams);
    let max_visible = list_area.height as usize;
    let scroll = if user_sel < max_visible {
        0
    } else {
        (user_sel + 1).saturating_sub(max_visible)
    };

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
                Style::default()
                    .fg(Color::LightCyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let name_style = if cursor {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
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
    state.select(
        if form.results.is_empty() || form.selected < n_teams || form.save_name.is_some() {
            None
        } else {
            Some(visible_sel)
        },
    );
    let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));
    f.render_stateful_widget(list, list_area, &mut state);
}

fn draw_detail(f: &mut Frame, area: Rect, app: &App) {
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(" detail · tab to switch panes ");
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
    // PR link box: shown only when the ticket is tied to a GitHub PR. Fixed
    // 3 rows (border + url).
    let pr_link_visible = app.detail_pr_link.is_some();
    let pr_link_h: u16 = if pr_link_visible { 3 } else { 0 };
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
    // PR comments pane (GitHub side). Hidden entirely when the ticket isn't
    // tied to a PR. Compact when unfocused; expands to half the inner area
    // when Tab brings focus to it (PRs can have very long threads).
    let pr_comments_focused = app.detail_focus == DetailFocus::PrComments;
    let pr_comments_visible = !app.pr_comments.is_empty();
    let pr_comments_h: u16 = if !pr_comments_visible {
        0
    } else if pr_comments_focused {
        (inner.height / 2).max(8)
    } else {
        // ~3 rows per comment (header + 1-2 body lines), capped small when unfocused.
        ((app.pr_comments.len() as u16) * 3 + 2).clamp(5, 10)
    };
    let mut constraints: Vec<Constraint> = Vec::with_capacity(6);
    constraints.push(Constraint::Min(8));
    constraints.push(Constraint::Length(projects_h));
    if pr_link_visible {
        constraints.push(Constraint::Length(pr_link_h));
    }
    if !is_subtask {
        constraints.push(Constraint::Length(subtasks_h));
    }
    constraints.push(comments_constraint);
    if pr_comments_visible {
        constraints.push(Constraint::Length(pr_comments_h));
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    let mut idx = 0;
    draw_detail_info(f, chunks[idx], app, t);
    idx += 1;
    draw_detail_projects(f, chunks[idx], app);
    idx += 1;
    if pr_link_visible {
        draw_detail_pr_link(f, chunks[idx], app);
        idx += 1;
    }
    if !is_subtask {
        draw_detail_subtasks(f, chunks[idx], app, t);
        idx += 1;
    }
    draw_detail_comments(f, chunks[idx], app);
    idx += 1;
    if pr_comments_visible {
        draw_detail_pr_comments(f, chunks[idx], app);
    }
}

fn draw_detail_pr_link(f: &mut Frame, area: Rect, app: &App) {
    let Some(url) = &app.detail_pr_link else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" pull request ");
    let inner = block.inner(area);
    f.render_widget(block, area);
    let line = Line::from(vec![Span::styled(
        url.clone(),
        Style::default()
            .fg(Color::Rgb(80, 160, 255))
            .add_modifier(Modifier::UNDERLINED),
    )]);
    f.render_widget(Paragraph::new(line), inner);
}

fn visible_subtask_count(app: &App, t: &jui_core::ticket::Ticket) -> usize {
    if app.show_archived_subtasks {
        t.subtasks.len()
    } else {
        t.subtasks
            .iter()
            .filter(|s| !is_subtask_archived(s))
            .count()
    }
}

fn is_subtask_archived(s: &jui_core::ticket::SubtaskRef) -> bool {
    let status = s.status.as_deref().unwrap_or("").to_ascii_lowercase();
    matches!(
        status.as_str(),
        "resolved"
            | "done"
            | "closed"
            | "archive"
            | "archived"
            | "won't do"
            | "wont do"
            | "cancelled"
            | "canceled"
    )
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
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM)
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
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(if focused { "▶ " } else { "  " });
    f.render_stateful_widget(list, inner, &mut state);
}

fn focus_border(focused: bool) -> Style {
    if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
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
            Span::styled(
                t.key.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(t.status.clone(), Style::default().fg(Color::Green)),
            Span::raw("  "),
            Span::styled(
                t.priority.clone().unwrap_or_default(),
                Style::default().fg(Color::Magenta),
            ),
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
                t.original_estimate_seconds
                    .map(fmt_seconds)
                    .unwrap_or_else(|| "—".into()),
                t.remaining_estimate_seconds
                    .map(fmt_seconds)
                    .unwrap_or_else(|| "—".into()),
                t.time_spent_seconds
                    .map(fmt_seconds)
                    .unwrap_or_else(|| "—".into()),
            )),
            Span::styled("(w to edit)", Style::default().fg(Color::DarkGray)),
        ]),
        Line::from(vec![
            Span::styled("dates: ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(
                "created {} · updated {}",
                t.created
                    .as_deref()
                    .map(fmt_date)
                    .unwrap_or_else(|| "—".into()),
                t.updated
                    .as_deref()
                    .map(fmt_date)
                    .unwrap_or_else(|| "—".into()),
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
        state.select(Some(
            app.linked_project_selected
                .min(app.detail_linked_projects.len() - 1),
        ));
    }
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(if focused { "▶ " } else { "  " });
    f.render_stateful_widget(list, inner, &mut state);
}

fn project_link_item(item: &DetailLinkedProject, pending_unlink: bool) -> ListItem<'_> {
    let p = &item.project;
    let suggested = item.state == "suggested";
    let worktree = item.state == "worktree";
    let no_match = item.state == "no_match";
    if no_match {
        let spans = vec![
            Span::styled(
                " ~ ",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "claude found no clear match",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
            Span::styled(
                "  (d to dismiss this notice)",
                Style::default().fg(Color::DarkGray),
            ),
        ];
        return ListItem::new(Line::from(spans));
    }
    let (marker, mstyle) = if worktree {
        (
            "◆",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )
    } else if suggested {
        (
            "?",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
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
        .or_else(|| {
            p.path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| p.path.display().to_string());
    let label_style = if worktree {
        Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD)
    } else if suggested {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let path_style = if worktree {
        Style::default().fg(Color::Magenta)
    } else if suggested {
        Style::default().fg(Color::Yellow)
    } else if p.available {
        Style::default()
    } else {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM)
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
    if worktree {
        spans.push(Span::styled(
            "  (existing ticket worktree)",
            Style::default().fg(Color::Magenta),
        ));
    }
    if !p.available && !suggested {
        spans.push(Span::styled(
            "  (unavailable)",
            Style::default().fg(Color::Red),
        ));
    }
    if pending_unlink {
        spans.push(Span::styled(
            "  press 'd' again to unlink",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn draw_detail_pr_comments(f: &mut Frame, area: Rect, app: &App) {
    let focused = app.detail_focus == DetailFocus::PrComments;
    let visible_idxs = app.visible_pr_comments();
    let hidden = app.hidden_pr_comment_count();
    let visible_count = visible_idxs.len();
    let pr_url = app
        .pr_comments
        .first()
        .map(|c| c.pr_url.clone())
        .unwrap_or_default();
    let title_count = if hidden > 0 {
        format!("{visible_count} · {hidden} hidden")
    } else {
        format!("{visible_count}")
    };
    let title = if pr_url.is_empty() {
        format!(" PR comments ({}) ", title_count)
    } else {
        format!(" PR comments ({}) · {} ", title_count, pr_url)
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(focus_border(focused))
        .title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.pr_comments.is_empty() {
        return;
    }
    if visible_idxs.is_empty() {
        // All comments are resolved + hidden — point the user at the toggle.
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("  {hidden} resolved · press H to show"),
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )),
            inner,
        );
        return;
    }

    // Compute scroll so the selected comment is visible. We render headers +
    // wrapped body lines until the area fills.
    let viewport_h = inner.height as usize;
    let wrap_w = inner.width.saturating_sub(2) as usize;
    let mut all: Vec<Line> = Vec::new();
    // Track which *visible-list* index each rendered line belongs to.
    let mut owner: Vec<usize> = Vec::new();
    for (visible_i, &real_idx) in visible_idxs.iter().enumerate() {
        let c = &app.pr_comments[real_idx];
        let resolved = c.is_resolved;
        let is_reply = !c.in_reply_to_id.is_empty();
        let selected = focused && visible_i == app.pr_comment_selected;
        let header_style = if selected {
            Style::default()
                .bg(Color::Rgb(60, 60, 80))
                .add_modifier(Modifier::BOLD)
        } else if resolved {
            // Dim resolved threads so they recede when the toggle is on.
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else if is_reply {
            Style::default()
                .fg(Color::Rgb(140, 110, 180))
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Rgb(180, 130, 220))
                .add_modifier(Modifier::BOLD)
        };
        let body_style = if resolved {
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC)
        } else {
            Style::default()
        };
        let date = c.created.split('T').next().unwrap_or(&c.created);
        // Indent threaded replies one column-set so they read as nested.
        let header_indent = if is_reply { "    " } else { "" };
        let body_indent = if is_reply { "      " } else { "  " };
        let arrow = if is_reply { "↳ " } else { "" };
        let resolved_tag = if resolved { "✓ " } else { "" };
        all.push(Line::from(vec![
            Span::styled(
                format!("{header_indent}{arrow}{resolved_tag}@{} ", c.author),
                header_style,
            ),
            Span::styled(format!("· {}", date), Style::default().fg(Color::DarkGray)),
        ]));
        owner.push(visible_i);
        for ln in c.body.lines() {
            for chunk in wrap_line(ln, wrap_w.max(20)) {
                all.push(Line::from(Span::styled(
                    format!("{body_indent}{}", chunk),
                    body_style,
                )));
                owner.push(visible_i);
            }
        }
        all.push(Line::from(""));
        owner.push(visible_i);
    }

    // Pick the start line so the selected comment's header is in view.
    let target_first_line = owner
        .iter()
        .position(|&o| o == app.pr_comment_selected)
        .unwrap_or(0);
    let max_scroll = all.len().saturating_sub(viewport_h);
    let scroll = target_first_line.min(max_scroll);
    let end = (scroll + viewport_h).min(all.len());
    let visible: Vec<Line> = all[scroll..end].to_vec();
    f.render_widget(Paragraph::new(visible), inner);
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

fn comment_item<'a>(
    c: &'a Comment,
    width: usize,
    mine: bool,
    pending_delete: bool,
) -> ListItem<'a> {
    let date = fmt_date(&c.created);
    let parsed = parse_reply(&c.body);

    let mine_tag: Vec<Span> = if mine {
        let style = if pending_delete {
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Green)
        };
        let label = if pending_delete {
            "  (press 'd' again to delete)"
        } else {
            "  (you)"
        };
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
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
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
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
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
    let Mode::Create(form) = &app.mode else {
        return;
    };
    let mut lines: Vec<Line> = Vec::new();
    if let Some(parent) = &form.parent {
        lines.push(Line::from(vec![
            Span::styled("↳ subtask of ", Style::default().fg(Color::Cyan)),
            Span::styled(
                parent.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));
    }
    lines.push(field_line("project    ", &form.project, form.field == 0));
    lines.push(field_line("type       ", &form.issue_type, form.field == 1));
    lines.push(field_line("summary    ", &form.summary, form.field == 2));
    lines.push(field_line(
        "description",
        &form.description,
        form.field == 3,
    ));
    lines.push(field_line(
        "estimate   ",
        &form.time_estimate,
        form.field == 4,
    ));
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
                    Style::default()
                        .bg(Color::Rgb(60, 60, 80))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let prefix = if i == form.assignee_picker_selected {
                    "  ▶ "
                } else {
                    "    "
                };
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
    let title = if form.parent.is_some() {
        " create subtask "
    } else {
        " create "
    };
    let p = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(p, area);
}

fn draw_edit(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Edit(form) = &app.mode else { return };
    let block = Block::default().borders(Borders::ALL).title(" edit ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let has_suggestion = form.suggestion.is_some();
    let (desc_h, sugg_h, sep_h) = if has_suggestion {
        // Split the remaining vertical space roughly 50/50 between the user's body
        // and the Claude suggestion, with a 2-line separator/header.
        let avail = inner.height.saturating_sub(7);
        let half = avail / 2;
        (half.max(1), avail.saturating_sub(half).max(1), 2)
    } else {
        (inner.height.saturating_sub(7).max(1), 0, 0)
    };

    let mut constraints: Vec<Constraint> = vec![
        Constraint::Length(1), // title
        Constraint::Length(1), // blank
        Constraint::Length(1), // summary field
        Constraint::Length(1), // blank
        Constraint::Length(1), // description label
        Constraint::Length(desc_h),
    ];
    if has_suggestion {
        constraints.push(Constraint::Length(sep_h)); // separator + label
        constraints.push(Constraint::Length(sugg_h));
    }
    constraints.push(Constraint::Length(1)); // hint

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    f.render_widget(Paragraph::new(format!("editing {}", form.key)), chunks[0]);
    // Render summary with the caret inline when active (avoid the trailing
    // caret `field_line` adds — the inline one already shows position).
    let summary_active = form.field == 0;
    let summary_text = if summary_active {
        insert_caret(&form.summary, form.summary_cursor)
    } else {
        form.summary.clone()
    };
    let summary_style = if summary_active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("summary       ", Style::default().fg(Color::DarkGray)),
            Span::styled(summary_text, summary_style),
        ])),
        chunks[2],
    );
    let desc_label_active = form.field == 1;
    let desc_label_style = if desc_label_active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let mut label_spans: Vec<Span> = vec![Span::styled("description:", desc_label_style)];
    if app.pending_improve.is_some() {
        label_spans.push(Span::raw("  "));
        label_spans.push(Span::styled(
            "(asking claude to tighten…)",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(label_spans)), chunks[4]);
    let body_text = if desc_label_active && !has_suggestion {
        insert_caret(&form.description, form.description_cursor)
    } else {
        form.description.clone()
    };
    f.render_widget(
        Paragraph::new(body_text).wrap(Wrap { trim: false }),
        chunks[5],
    );

    let hint_idx = if has_suggestion {
        let sep_lines = vec![
            Line::from(""),
            Line::from(Span::styled(
                "claude suggestion (y: accept · n: reject):",
                Style::default()
                    .fg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            )),
        ];
        f.render_widget(
            Paragraph::new(sep_lines).wrap(Wrap { trim: false }),
            chunks[6],
        );
        let sugg = form.suggestion.clone().unwrap_or_default();
        f.render_widget(
            Paragraph::new(Span::styled(sugg, Style::default().fg(Color::Magenta)))
                .wrap(Wrap { trim: false }),
            chunks[7],
        );
        8
    } else {
        6
    };

    let hint_text = if has_suggestion {
        "y: keep claude rewrite   n/esc: reject   (then ctrl+s/F5 to save)"
    } else {
        "tab: switch · ←→↑↓/home/end: move · del: forward · enter: save (sum) / newline (desc) · ctrl+e: $EDITOR · ctrl+r/F6: claude tighten · ctrl+s/F5: save · esc: cancel"
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            hint_text,
            Style::default().fg(Color::DarkGray),
        )),
        chunks[hint_idx],
    );
}

fn draw_comment(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Comment(form) = &app.mode else {
        return;
    };
    let title = if form.reply_to.is_some() {
        format!(" reply on {} ", form.key)
    } else {
        format!(" comment on {} ", form.key)
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    // If this is a reply, show the parent excerpt up top so the user has context.
    let (top_h, has_quote) = if form.reply_to.is_some() {
        (4, true)
    } else {
        (1, false)
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(top_h),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    if has_quote {
        let ctx = form.reply_to.as_ref().unwrap();
        let excerpt = ctx
            .parent_body
            .lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        let lines = vec![
            Line::from(vec![
                Span::styled("↳ replying to ", Style::default().fg(Color::Cyan)),
                Span::styled(
                    ctx.parent_author.clone(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
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
    let Mode::TicketProjects(form) = &app.mode else {
        return;
    };
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
            let is_worktree = item.state == "worktree";
            let check = if is_worktree {
                "[w]"
            } else if item.linked {
                "[x]"
            } else {
                "[ ]"
            };
            let check_style = if item.linked {
                let color = if is_worktree {
                    Color::Magenta
                } else {
                    Color::Green
                };
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let path_style = if item.project.available {
                Style::default()
            } else {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM)
            };
            let mut spans = vec![
                Span::styled(format!(" {check} "), check_style),
                Span::styled(
                    format!("{:<3}", item.project.kind),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw("  "),
                Span::styled(item.project.path.display().to_string(), path_style),
            ];
            if let Some(nick) = &item.project.nickname {
                spans.push(Span::styled(
                    format!("  ({nick})"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if !item.project.available {
                spans.push(Span::styled(
                    "  (unavailable)",
                    Style::default().fg(Color::Red),
                ));
            }
            if is_worktree {
                spans.push(Span::styled(
                    "  (existing ticket worktree)",
                    Style::default().fg(Color::Magenta),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(form.selected.min(form.items.len() - 1)));
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, inner, &mut state);
}

fn draw_active_status_config(f: &mut Frame, area: Rect, app: &App) {
    let Mode::ActiveStatusConfig(form) = &app.mode else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" active workflow statuses ({}) ", form.items.len()));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Reserve the bottom row for the add-input box (when adding) and the hint.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);

    if form.items.is_empty() {
        let p = Paragraph::new(Span::styled(
            "no active statuses — press 'i' to add one",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(p, chunks[0]);
    } else {
        let items: Vec<ListItem> = form
            .items
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let pending = form.pending_remove == Some(i);
                let mut spans: Vec<Span> = vec![
                    Span::styled(" ● ", Style::default().fg(Color::Green)),
                    Span::styled(s.clone(), Style::default()),
                ];
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
        if !form.items.is_empty() {
            state.select(Some(form.selected.min(form.items.len() - 1)));
        }
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▶ ");
        let mut s = state;
        f.render_stateful_widget(list, chunks[0], &mut s);
    }

    // Add-input row (only when adding).
    if let Some(buf) = &form.adding {
        let line = Line::from(vec![
            Span::styled(
                " new: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(buf.clone(), Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("▏", Style::default().fg(Color::Cyan)),
        ]);
        f.render_widget(Paragraph::new(line), chunks[1]);
    }

    let hint = if form.adding.is_some() {
        "enter: add · esc: cancel"
    } else {
        "j/k: move · i: add · d×2: delete · esc/q: back   (saved on every change)"
    };
    f.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
        chunks[2],
    );
}

fn draw_settings(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Settings(form) = &app.mode else {
        return;
    };
    let block = Block::default().borders(Borders::ALL).title(" settings ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows: [(&str, &str); 5] = [
        ("Default create status", form.default_create_status.as_str()),
        (
            "All-mine exclude status",
            form.all_mine_exclude_status.as_str(),
        ),
        ("PR submit status", form.pr_submit_status.as_str()),
        ("Code assistant", form.code_assistant.as_str()),
        (
            "Claude permission mode",
            form.claude_permission_mode.as_str(),
        ),
    ];

    let mut items: Vec<ListItem> = Vec::new();
    for (label, value) in rows.iter() {
        let value_span = if value.is_empty() {
            Span::styled("(empty)", Style::default().fg(Color::DarkGray))
        } else {
            Span::styled(
                (*value).to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            )
        };
        let line = Line::from(vec![
            Span::styled(format!(" {label:<26} "), Style::default().fg(Color::Gray)),
            value_span,
        ]);
        items.push(ListItem::new(line));
    }

    let mut state = ListState::default();
    state.select(Some(form.selected.min(rows.len() - 1)));
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(inner);
    let mut s = state;
    f.render_stateful_widget(list, chunks[0], &mut s);

    let help_line = match form.selected {
        0 => "applied as a post-create transition after a new ticket is created",
        1 => {
            "JQL clause: assignee = currentUser() AND status != \"<this>\" when the M-toggle is on"
        }
        3 => "assistant launched in tmux from start-work, implementation, and DevQA panes",
        4 => "default --permission-mode for Claude on start-work (ignored by opencode)",
        _ => "",
    };
    f.render_widget(
        Paragraph::new(Span::styled(
            help_line,
            Style::default().fg(Color::DarkGray),
        )),
        chunks[1],
    );

    let hint = if form.picker.is_some() {
        "type: filter · ↑/↓: move · enter: pick · esc: cancel"
    } else {
        "j/k: move · i/enter: edit · esc/q: back   (saved on every change)"
    };
    f.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
        chunks[2],
    );

    if form.picker.is_some() {
        draw_settings_picker(f, area, app);
    }
}

fn draw_rules(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Rules(form) = &app.mode else { return };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" rules — a add · d delete · t toggle · enter edit · esc back ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    if form.items.is_empty() {
        let p = Paragraph::new(Span::styled(
            "no rules yet. press `a` to add one.",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(p, inner);
        return;
    }

    let items: Vec<ListItem> = form
        .items
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let enabled = if r.enabled {
                Span::styled("[on] ", Style::default().fg(Color::Green))
            } else {
                Span::styled("[off]", Style::default().fg(Color::DarkGray))
            };
            let pending = form.pending_remove == Some(i);
            let name_style = if pending {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else {
                Style::default().add_modifier(Modifier::BOLD)
            };
            let trigger = trigger_summary(&r.trigger);
            let summary = format!(
                "  {}  on {}  · {} cond · {} act",
                r.name,
                trigger,
                r.conditions.len(),
                r.actions.len()
            );
            ListItem::new(Line::from(vec![enabled, Span::styled(summary, name_style)]))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(form.selected.min(form.items.len().saturating_sub(1))));
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, inner, &mut state);
}

fn draw_home(f: &mut Frame, area: Rect, app: &App) {
    use crate::app::{HomeFocus, HOME_TARGETS};
    let Mode::Home(form) = &app.mode else { return };
    // Size the left menu to the widest entry so view options never truncate.
    // Each row is " {num} " + "({hot}) " + desc, plus a 2-col highlight symbol
    // ("▶ ") and 2 cols of borders.
    let menu_content_w = HOME_TARGETS
        .iter()
        .map(|(_, num, hot, desc)| {
            format!(" {num} ").chars().count()
                + format!("({hot}) ").chars().count()
                + desc.chars().count()
        })
        .max()
        .unwrap_or(0) as u16;
    // Reserve room for the feed but always favor showing the full menu text.
    // Pad the fitted width by 20% for a roomier menu pane.
    let menu_w = ((menu_content_w + 4) * 6 / 5)
        .min(area.width.saturating_sub(20))
        .max(20);
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(menu_w), Constraint::Min(20)])
        .split(area);

    // Left: shortcut menu.
    let menu_focused = form.focus == HomeFocus::Menu;
    let menu_block = Block::default()
        .borders(Borders::ALL)
        .title(if menu_focused { " ▸ jui " } else { " jui " });
    let menu_inner = menu_block.inner(chunks[0]);
    f.render_widget(menu_block, chunks[0]);

    let items: Vec<ListItem> = HOME_TARGETS
        .iter()
        .map(|(_, num, hot, desc)| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {num} "),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("({hot}) "), Style::default().fg(Color::DarkGray)),
                Span::raw(*desc),
            ]))
        })
        .collect();
    let mut menu_state = ListState::default();
    menu_state.select(Some(form.menu_selected.min(HOME_TARGETS.len() - 1)));
    let menu_list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(menu_list, menu_inner, &mut menu_state);

    // Right: activity feed.
    let feed_focused = form.focus == HomeFocus::Activity;
    let feed_block = Block::default()
        .borders(Borders::ALL)
        .title(if feed_focused {
            format!(" ▸ recent activity ({}) ", form.items.len())
        } else {
            format!(" recent activity ({}) ", form.items.len())
        });
    let feed_inner = feed_block.inner(chunks[1]);
    f.render_widget(feed_block, chunks[1]);

    if form.loading {
        f.render_widget(
            Paragraph::new(Span::styled(
                "loading…",
                Style::default().fg(Color::DarkGray),
            )),
            feed_inner,
        );
        return;
    }
    if let Some(err) = &form.error {
        f.render_widget(
            Paragraph::new(format!("err: {err}")).style(Style::default().fg(Color::Red)),
            feed_inner,
        );
        return;
    }
    if form.items.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "no recent activity yet — comments and status changes will land here.",
                Style::default().fg(Color::DarkGray),
            )),
            feed_inner,
        );
        return;
    }
    let feed_items: Vec<ListItem> = form
        .items
        .iter()
        .map(|e| {
            let kind_span = match e.kind.as_str() {
                "jira_comment" => Span::styled(" jira ", Style::default().fg(Color::Cyan)),
                "pr_comment" => Span::styled(" pr   ", Style::default().fg(Color::Magenta)),
                "status_change" => Span::styled(" stat ", Style::default().fg(Color::Yellow)),
                _ => Span::styled(format!(" {:5}", e.kind), Style::default().fg(Color::Gray)),
            };
            let when = format_home_ts(&e.when);
            let mut spans: Vec<Span> = vec![
                Span::styled(format!("{when} "), Style::default().fg(Color::DarkGray)),
                kind_span,
                Span::raw(" "),
            ];
            if let Some(k) = &e.ticket_key {
                spans.push(Span::styled(
                    format!("{k} "),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::raw(e.summary.clone()));
            if let Some(d) = &e.detail {
                let snippet = one_line(d);
                if !snippet.is_empty() {
                    spans.push(Span::styled(
                        format!("  — {snippet}"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut feed_state = ListState::default();
    feed_state.select(Some(form.selected.min(form.items.len() - 1)));
    let feed_list = List::new(feed_items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(feed_list, feed_inner, &mut feed_state);
}

/// Best-effort RFC3339 → short timestamp. Same-day entries show HH:MM; older
/// ones include MM-DD. Falls back to the raw string on parse failure.
fn format_home_ts(raw: &str) -> String {
    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) {
        let local = parsed.with_timezone(&chrono::Local);
        let now = chrono::Local::now();
        if local.date_naive() == now.date_naive() {
            return local.format("%H:%M").to_string();
        }
        return local.format("%m-%d %H:%M").to_string();
    }
    raw.chars().take(16).collect()
}

fn draw_rule_log(f: &mut Frame, area: Rect, app: &App) {
    let Mode::RuleLog(form) = &app.mode else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" rule log (5-day rolling) — j/k scroll · r refresh · esc back ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    if form.loading {
        f.render_widget(Paragraph::new("loading…"), inner);
        return;
    }
    if let Some(err) = &form.error {
        f.render_widget(
            Paragraph::new(format!("err: {err}")).style(Style::default().fg(Color::Red)),
            inner,
        );
        return;
    }
    if form.items.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "no entries yet — rule fires will land here as they happen.",
                Style::default().fg(Color::DarkGray),
            )),
            inner,
        );
        return;
    }

    let items: Vec<ListItem> = form
        .items
        .iter()
        .map(|e| {
            let ts = format_rule_log_ts(e.fired_at);
            let status_span = if e.status == "ok" {
                Span::styled("✓", Style::default().fg(Color::Green))
            } else {
                Span::styled(
                    "✗",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )
            };
            let mut spans: Vec<Span> = vec![
                Span::styled(format!("{ts}  "), Style::default().fg(Color::DarkGray)),
                status_span,
                Span::raw(" "),
                Span::styled(
                    format!("{}  ", e.rule_name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("[{}] ", e.trigger_kind),
                    Style::default().fg(Color::Cyan),
                ),
            ];
            if let Some(k) = &e.ticket_key {
                spans.push(Span::styled(
                    format!("{k} "),
                    Style::default().fg(Color::Yellow),
                ));
            }
            spans.push(Span::styled(
                format!("→ {}", e.action_kind),
                Style::default().fg(Color::Gray),
            ));
            if let Some(t) = &e.action_target {
                spans.push(Span::raw(": "));
                spans.push(Span::raw(t.clone()));
            }
            if let Some(c) = &e.conditions_summary {
                spans.push(Span::styled(
                    format!("  · cond: {c}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if e.status == "err" {
                if let Some(m) = &e.message {
                    spans.push(Span::styled(
                        format!("  · err: {}", one_line(m)),
                        Style::default().fg(Color::Red),
                    ));
                }
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let mut state = ListState::default();
    state.select(Some(form.selected.min(form.items.len().saturating_sub(1))));
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, inner, &mut state);
}

/// Format a unix-ms timestamp for the log column. Same-day entries show
/// only `HH:MM:SS`; older entries include the date so the 5-day window is
/// readable at a glance.
fn format_rule_log_ts(ms: i64) -> String {
    let dt = chrono::DateTime::<chrono::Local>::from(
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms).unwrap_or_default(),
    );
    let now = chrono::Local::now();
    if dt.date_naive() == now.date_naive() {
        dt.format("%H:%M:%S").to_string()
    } else {
        dt.format("%m-%d %H:%M:%S").to_string()
    }
}

fn trigger_summary(t: &jui_core::rules::Trigger) -> String {
    use jui_core::rules::Trigger as T;
    match t {
        T::PrCreated => "pr_created".into(),
        T::StartWork => "start_work".into(),
        T::StopWork => "stop_work".into(),
        T::TicketStatusChanged { from, to } => match (from, to) {
            (None, None) => "ticket_status_changed".into(),
            (Some(f), None) => format!("ticket_status_changed (from {f})"),
            (None, Some(t)) => format!("ticket_status_changed → {t}"),
            (Some(f), Some(t)) => format!("ticket_status_changed {f} → {t}"),
        },
        T::TicketAssigned { to_me } => match to_me {
            Some(true) => "ticket_assigned (to me)".into(),
            Some(false) => "ticket_assigned (not me)".into(),
            None => "ticket_assigned".into(),
        },
    }
}

fn condition_summary(c: &jui_core::rules::Condition) -> String {
    use jui_core::rules::Condition as C;
    match c {
        C::ProjectKeyEquals { value } => format!("project_key = {value}"),
        C::StatusEquals { value } => format!("status = {value}"),
        C::IssueTypeIn { values } => format!("issue_type in [{}]", values.join(", ")),
        C::HasLinkedRepo => "has_linked_repo".into(),
        C::ActorIsMe => "actor_is_me".into(),
    }
}

fn action_summary(a: &jui_core::rules::Action) -> String {
    use jui_core::rules::Action as A;
    match a {
        A::JiraTransition { to } => format!("jira transition → {to}"),
        A::JiraComment { body } => format!("jira comment: {}", one_line(body)),
        A::GithubPrComment { body } => format!("github pr comment: {}", one_line(body)),
        A::SetTicketDevQa => "set ticket DevQA from PR pick".into(),
    }
}

fn one_line(s: &str) -> String {
    let s = s.replace('\n', "↵");
    // Char-aware truncate: `String::truncate` panics if the byte index lands
    // mid-codepoint (e.g. inside `↵` or any non-ASCII rune).
    let mut out: String = s.chars().take(60).collect();
    if out.chars().count() < s.chars().count() {
        out.push('…');
    }
    out
}

fn draw_rule_edit(f: &mut Frame, area: Rect, app: &App) {
    use crate::app::{rule_edit_rows, RuleEditTarget, RuleRow};
    let Mode::RuleEdit(form) = &app.mode else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" edit rule — {} ", form.rule.name));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = rule_edit_rows(&form.rule);
    let trigger_filter_text = match &form.rule.trigger {
        jui_core::rules::Trigger::TicketStatusChanged { to, .. } => {
            to.clone().unwrap_or_else(|| "(any to_status)".into())
        }
        jui_core::rules::Trigger::TicketAssigned { to_me } => match to_me {
            Some(true) => "true".into(),
            Some(false) => "false".into(),
            None => "(any)".into(),
        },
        _ => String::new(),
    };

    let mut lines: Vec<Line> = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        let selected = i == form.selected_row;
        let in_edit = selected && form.edit_buffer.is_some();
        let bullet = if selected { "▶ " } else { "  " };
        let row_style = if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let (label, value): (String, String) = match *row {
            RuleRow::Name => ("name        ".into(), form.rule.name.clone()),
            RuleRow::Trigger => ("trigger     ".into(), trigger_summary(&form.rule.trigger)),
            RuleRow::TriggerFilter => ("filter      ".into(), trigger_filter_text.clone()),
            RuleRow::Enabled => (
                "enabled     ".into(),
                if form.rule.enabled {
                    "yes".into()
                } else {
                    "no".into()
                },
            ),
            RuleRow::ConditionsHeader => (String::new(), "─── conditions ───".into()),
            RuleRow::Cond(idx) => match form.rule.conditions.get(idx) {
                Some(c) => {
                    let pending = form.pending_remove_condition == Some(idx);
                    let prefix = if pending { "(d again) " } else { "" };
                    (
                        format!("cond[{idx}]    "),
                        format!("{prefix}{}", condition_summary(c)),
                    )
                }
                None => (format!("cond[{idx}]    "), "(missing)".into()),
            },
            RuleRow::AddCondition => ("            ".into(), "+ add condition (a / enter)".into()),
            RuleRow::ActionsHeader => (String::new(), "─── actions ───".into()),
            RuleRow::Act(idx) => match form.rule.actions.get(idx) {
                Some(a) => {
                    let pending = form.pending_remove_action == Some(idx);
                    let prefix = if pending { "(d again) " } else { "" };
                    (
                        format!("act[{idx}]     "),
                        format!("{prefix}{}", action_summary(a)),
                    )
                }
                None => (format!("act[{idx}]     "), "(missing)".into()),
            },
            RuleRow::AddAction => ("            ".into(), "+ add action (a / enter)".into()),
        };
        let value_render = if in_edit {
            let buf = form.edit_buffer.as_deref().unwrap_or("");
            format!("{buf}▏")
        } else {
            value
        };
        if matches!(row, RuleRow::ConditionsHeader | RuleRow::ActionsHeader) {
            lines.push(Line::from(Span::styled(
                value_render,
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(vec![
                Span::styled(bullet, row_style),
                Span::styled(label, Style::default().fg(Color::Gray)),
                Span::styled(value_render, row_style),
            ]));
        }
    }
    if let Some(err) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    lines.push(Line::from(""));
    // Variable reference — surfaced in every editor session so users don't
    // have to remember (or grep the source) for placeholder names.
    let vars = "{ticket_key} {ticket_summary} {ticket_status} {project_key} {issue_type} \
                {from_status} {to_status} {pr_url} {pr_number} {pr_repo} \
                {reviewer_handle} {devqa_handle} {reviewer_account_id} {devqa_account_id} {actor}";
    lines.push(Line::from(Span::styled(
        "available variables:",
        Style::default().fg(Color::Cyan),
    )));
    lines.push(Line::from(Span::styled(
        vars,
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(""));
    let hint = if form.edit_buffer.is_some() {
        "typing… enter: commit · esc: cancel"
    } else {
        "j/k move · enter/e edit · tab/space cycle · a add · d delete (twice) · ctrl-s save · esc back"
    };
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(Color::DarkGray),
    )));
    // Avoid the unused-variant warning on RuleEditTarget — Rust thinks the
    // variant types are dead because we never destructure them in `ui.rs`.
    let _ = std::mem::size_of::<RuleEditTarget>();
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);

    if form.picker.is_some() {
        draw_rule_edit_picker(f, area, app);
    }
    if form.var_picker.is_some() {
        draw_var_picker(f, area, app);
    }
}

fn draw_var_picker(f: &mut Frame, parent: Rect, app: &App) {
    use crate::app::filter_vars;
    let Mode::RuleEdit(form) = &app.mode else {
        return;
    };
    let Some(vp) = form.var_picker.as_ref() else {
        return;
    };
    let Some(buf) = form.edit_buffer.as_ref() else {
        return;
    };
    let filter = &buf[vp.anchor + 1..];
    let matches = filter_vars(filter);

    // Modest-sized popup anchored near top of the edit pane. Keeps it out of
    // the way of the field rows; user is reading from the buffer cursor area
    // anyway. Width is wide enough for "name — description".
    let w = parent.width.saturating_sub(4).min(70).max(40);
    let h = ((matches.len() as u16) + 3).clamp(5, 14);
    let x = parent.x + 2;
    let y = parent.y + 2;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    f.render_widget(ratatui::widgets::Clear, area);
    let title = if filter.is_empty() {
        " variables ".to_string()
    } else {
        format!(" variables — filter: {{{filter} ")
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let items: Vec<ListItem> = matches
        .iter()
        .map(|(name, desc)| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{{{name}}}"),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(*desc, Style::default().fg(Color::Gray)),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    if !matches.is_empty() {
        state.select(Some(vp.selected.min(matches.len() - 1)));
    }
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[0], &mut state);

    let hint =
        "tab/enter: insert · ↑/↓ move · esc: close (keep `{`) · backspace past `{`: close + delete";
    f.render_widget(
        Paragraph::new(Span::styled(hint, Style::default().fg(Color::DarkGray))),
        chunks[1],
    );
}

fn draw_rule_edit_picker(f: &mut Frame, parent: Rect, app: &App) {
    use crate::app::RulePickerTarget;
    let Mode::RuleEdit(form) = &app.mode else {
        return;
    };
    let Some(p) = form.picker.as_ref() else {
        return;
    };

    // Centered modal — same dimensions as the Settings picker for visual
    // consistency.
    let w = parent.width.saturating_sub(4).min(70).max(30);
    let h = parent.height.saturating_sub(4).min(20).max(8);
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    f.render_widget(ratatui::widgets::Clear, area);

    let title = match p.target {
        RulePickerTarget::ActionTransitionTo(_) => " pick transition target status ",
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(inner);

    let filter_line = Line::from(vec![
        Span::styled(" filter: ", Style::default().fg(Color::Cyan)),
        Span::styled(
            p.query.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("▏", Style::default().fg(Color::Cyan)),
    ]);
    let state_line = if p.loading {
        Line::from(Span::styled(
            " loading…",
            Style::default().fg(Color::DarkGray),
        ))
    } else if let Some(err) = &p.error {
        Line::from(Span::styled(
            format!(" err: {err}"),
            Style::default().fg(Color::Red),
        ))
    } else {
        let n = p.filtered().len();
        Line::from(Span::styled(
            format!(" {n} match{}", if n == 1 { "" } else { "es" }),
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(vec![filter_line, state_line]), chunks[0]);

    let matches: Vec<&str> = p.filtered();
    let items: Vec<ListItem> = matches
        .iter()
        .map(|s| ListItem::new(Span::raw((*s).to_string())))
        .collect();
    let mut list_state = ListState::default();
    if !matches.is_empty() {
        list_state.select(Some(p.selected.min(matches.len() - 1)));
    }
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[1], &mut list_state);
}

fn draw_settings_picker(f: &mut Frame, parent: Rect, app: &App) {
    let Mode::Settings(form) = &app.mode else {
        return;
    };
    let Some(p) = form.picker.as_ref() else {
        return;
    };

    // Centered modal: 60% × 70% of the settings area, clamped.
    let w = parent.width.saturating_sub(4).min(70).max(30);
    let h = parent.height.saturating_sub(4).min(20).max(8);
    let x = parent.x + (parent.width.saturating_sub(w)) / 2;
    let y = parent.y + (parent.height.saturating_sub(h)) / 2;
    let area = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    // Clear behind so we don't render on top of the list rows.
    f.render_widget(ratatui::widgets::Clear, area);

    let title = match p.row {
        0 => " pick default create status ",
        1 => " pick all-mine exclude status ",
        3 => " pick Claude permission mode ",
        _ => " pick status ",
    };
    let block = Block::default().borders(Borders::ALL).title(title);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(2), Constraint::Min(1)])
        .split(inner);

    // Top: filter input + status line.
    let filter_line = Line::from(vec![
        Span::styled(" filter: ", Style::default().fg(Color::Cyan)),
        Span::styled(
            p.query.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("▏", Style::default().fg(Color::Cyan)),
    ]);
    let state_line = if p.loading {
        Line::from(Span::styled(
            " loading…",
            Style::default().fg(Color::DarkGray),
        ))
    } else if let Some(err) = &p.error {
        Line::from(Span::styled(
            format!(" err: {err}"),
            Style::default().fg(Color::Red),
        ))
    } else {
        let n = p.filtered().len();
        Line::from(Span::styled(
            format!(" {n} match{}", if n == 1 { "" } else { "es" }),
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(vec![filter_line, state_line]), chunks[0]);

    // Bottom: scrollable list.
    let matches: Vec<&str> = p.filtered();
    let items: Vec<ListItem> = matches
        .iter()
        .map(|s| ListItem::new(Span::raw((*s).to_string())))
        .collect();
    let mut list_state = ListState::default();
    if !matches.is_empty() {
        list_state.select(Some(p.selected.min(matches.len() - 1)));
    }
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, chunks[1], &mut list_state);
}

fn draw_projects(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Projects(form) = &app.mode else {
        return;
    };
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
            let pending =
                form.pending_remove.as_deref() == Some(p.path.as_path()) && i == form.selected;
            let (marker, marker_style) = if !p.available {
                ("✗", Style::default().fg(Color::Red))
            } else if p.kind == "git" {
                ("●", Style::default().fg(Color::Green))
            } else {
                ("●", Style::default().fg(Color::Cyan))
            };
            let kind = format!("{:<3}", p.kind);
            let path_style = if !p.available {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::DIM)
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
                spans.push(Span::styled(
                    format!("  ({nick})"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if !p.available {
                spans.push(Span::styled(
                    "  (unavailable)",
                    Style::default().fg(Color::Red),
                ));
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
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, inner, &mut state);
}

fn draw_projects_add(f: &mut Frame, area: Rect, app: &App) {
    let Mode::ProjectsAdd(form) = &app.mode else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" add project ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let header = Paragraph::new(Line::from(vec![
        Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}▏", form.query),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    f.render_widget(header, chunks[0]);

    if form.loading {
        let p =
            Paragraph::new("scanning $HOME for repos…").style(Style::default().fg(Color::DarkGray));
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
        state.select(if items.is_empty() {
            None
        } else {
            Some(form.selected.min(items.len() - 1))
        });
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
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
    let Mode::Implementation(form) = &app.mode else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" claude implementation — {} ", form.key));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

    let projects = if form.project_paths.is_empty() {
        "—".to_string()
    } else {
        form.project_paths
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" · ")
    };
    let header_text = format!(
        "projects: {projects}\nupdated: {}",
        if form.updated_at.is_empty() {
            "—".into()
        } else {
            form.updated_at.clone()
        }
    );
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
    let Mode::StartWorkPrompt(form) = &app.mode else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" start work — {} ", form.ticket_key));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(Span::styled(
            "configure the start-work checkout, then submit:",
            Style::default().fg(Color::Cyan),
        )),
        Line::from(""),
    ];

    // Location row (always shown).
    let loc_selected = form.field == 0;
    let label_style = if loc_selected {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let wt_active = matches!(form.location, jui_core::scm::WorkLocation::Worktree);
    let br_active = matches!(form.location, jui_core::scm::WorkLocation::BranchInRepo);
    let opt_style = |active: bool| {
        if active {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    };
    lines.push(Line::from(vec![
        Span::styled("location  ", label_style),
        Span::styled(
            if wt_active {
                "[●] worktree"
            } else {
                "[ ] worktree"
            },
            opt_style(wt_active),
        ),
        Span::raw("   "),
        Span::styled(
            if br_active {
                "[●] branch in repo"
            } else {
                "[ ] branch in repo"
            },
            opt_style(br_active),
        ),
    ]));
    if loc_selected {
        lines.push(Line::from(Span::styled(
            "  ←/→ or space to toggle. branch-in-repo aborts if working tree is dirty.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    let branch_style = if form.field == 1 {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let branch_text = if form.field == 1 {
        insert_caret(&form.branch_slug, form.branch_cursor)
    } else {
        form.branch_slug.clone()
    };
    lines.push(Line::from(vec![
        Span::styled("branch    ", Style::default().fg(Color::DarkGray)),
        Span::styled(branch_text, branch_style),
    ]));
    if form.field == 1 {
        lines.push(Line::from(Span::styled(
            "  edit before submit. spaces/punctuation are normalized to '-' for git.",
            Style::default().fg(Color::DarkGray),
        )));
    }

    if form.need_time {
        lines.push(field_line(
            "estimate  ",
            &form.time_estimate,
            form.field == 2,
        ));
        if form.field == 2 {
            lines.push(Line::from(Span::styled(
                "  examples: 8h, 2d 4h, 30m",
                Style::default().fg(Color::DarkGray),
            )));
        }
    }
    if form.need_priority {
        lines.push(field_line("priority  ", &form.priority, form.field == 3));
        if form.field == 3 {
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
    // Plan-mode toggle row (always shown, field index 4).
    let plan_selected = form.field == 4;
    let plan_label_style = if plan_selected {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    lines.push(Line::from(vec![
        Span::styled("plan mode ", plan_label_style),
        Span::styled(
            if form.plan_mode { "[●] on" } else { "[ ] on" },
            opt_style(form.plan_mode),
        ),
        Span::raw("   "),
        Span::styled(
            if form.plan_mode {
                "[ ] off"
            } else {
                "[●] off"
            },
            opt_style(!form.plan_mode),
        ),
    ]));
    if plan_selected {
        let detail = if form.plan_mode {
            "  ←/→ or space to toggle. for Claude, launches with --permission-mode plan."
                .to_string()
        } else {
            format!(
                "  ←/→ or space to toggle. off → uses configured default ({}).",
                if form.default_permission_mode.trim().is_empty() {
                    "Claude default"
                } else {
                    form.default_permission_mode.trim()
                }
            )
        };
        lines.push(Line::from(Span::styled(
            detail,
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Extra-shell toggle row (always shown, field index 5).
    let shell_selected = form.field == 5;
    let shell_label_style = if shell_selected {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    lines.push(Line::from(vec![
        Span::styled("shell pane", shell_label_style),
        Span::raw(" "),
        Span::styled(
            if form.open_shell_pane {
                "[●] yes"
            } else {
                "[ ] yes"
            },
            opt_style(form.open_shell_pane),
        ),
        Span::raw("   "),
        Span::styled(
            if form.open_shell_pane {
                "[ ] no"
            } else {
                "[●] no"
            },
            opt_style(!form.open_shell_pane),
        ),
    ]));
    if shell_selected {
        lines.push(Line::from(Span::styled(
            "  ←/→ or space to toggle. opens an extra shell pane in the worktree next to the assistant.",
            Style::default().fg(Color::DarkGray),
        )));
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

fn draw_devqa_prompt(f: &mut Frame, area: Rect, app: &App) {
    let Mode::DevQaPrompt(form) = &app.mode else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" begin dev qa — {} ", form.ticket_key));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let opt_style = |active: bool| {
        if active {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        }
    };
    let wt = form.use_worktree;

    let mut lines = vec![
        Line::from(Span::styled(
            "test this PR — choose where to check out its branch:",
            Style::default().fg(Color::Cyan),
        )),
        Line::from(Span::styled(
            format!("  {}", form.pr_url),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "checkout  ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if wt {
                    "[●] git worktree"
                } else {
                    "[ ] git worktree"
                },
                opt_style(wt),
            ),
            Span::raw("   "),
            Span::styled(
                if !wt {
                    "[●] branch in repo"
                } else {
                    "[ ] branch in repo"
                },
                opt_style(!wt),
            ),
        ]),
        Line::from(Span::styled(
            if wt {
                "  ←/→ or space to toggle. isolates the PR branch in a separate worktree."
                    .to_string()
            } else {
                "  ←/→ or space to toggle. checks the PR branch out in your clone (aborts if dirty)."
                    .to_string()
            },
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Claude launches to TEST the change, not to solve the ticket.",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    if let Some(err) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(Color::Red),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "←/→ or space: toggle   enter / F5 / ctrl+s: launch   esc: cancel",
        Style::default().fg(Color::DarkGray),
    )));
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

fn draw_devqa_resolve_confirm(f: &mut Frame, area: Rect, app: &App) {
    let Mode::DevQaResolveConfirm(form) = &app.mode else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" resolve dev qa — {} ", form.ticket_key));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(Span::styled(
            "Resolve DevQA — this posts to GitHub:",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(vec![
            Span::raw("  • comment "),
            Span::styled(
                "DevQA: Passed",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" on the PR"),
        ]),
        Line::from("  • 🚀 reaction on the PR's top comment"),
        Line::from("  • then advance the ticket past Dev QA (if a transition exists)"),
        Line::from("  • remove the DevQA worktree (if one was created)"),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", form.pr_url),
            Style::default().fg(Color::DarkGray),
        )),
    ];
    if let Some(err) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("error: {err}"),
            Style::default().fg(Color::Red),
        )));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "enter / y: post & advance    esc / n: cancel",
        Style::default().fg(Color::DarkGray),
    )));
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

fn draw_devqa_cleanup_confirm(f: &mut Frame, area: Rect, app: &App) {
    let Mode::DevQaCleanupConfirm(form) = &app.mode else {
        return;
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" remove dev qa worktree — {} ", form.ticket_key));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let lines = vec![
        Line::from(Span::styled(
            "The DevQA worktree has uncommitted changes.",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {}", form.detail),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(""),
        Line::from("Removing it will discard those changes. DevQA is already"),
        Line::from("resolved — this only affects the local worktree."),
        Line::from(""),
        Line::from(Span::styled(
            "enter / y: discard & remove    esc / n: keep the worktree",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let p = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(p, inner);
}

fn draw_edit_priority(f: &mut Frame, area: Rect, app: &App) {
    let Mode::EditPriority(form) = &app.mode else {
        return;
    };
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
            ListItem::new(Line::from(vec![
                priority_span(Some(name)),
                Span::raw(name.clone()),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(if form.options.is_empty() {
        None
    } else {
        Some(form.selected)
    });
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, inner, &mut state);
}

fn draw_edit_time(f: &mut Frame, area: Rect, app: &App) {
    let Mode::EditTime(form) = &app.mode else {
        return;
    };
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
    let Mode::Transition(form) = &app.mode else {
        return;
    };
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
        let p = Paragraph::new(format!("error: {err}")).style(Style::default().fg(Color::Red));
        f.render_widget(p, inner);
        return;
    }

    let items: Vec<ListItem> = form
        .options
        .iter()
        .map(|t| {
            let to = t.to_status.clone().unwrap_or_default();
            let line = Line::from(vec![
                Span::styled(
                    format!("{:<24}", t.name),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("→ {to}"), Style::default().fg(Color::Green)),
            ]);
            ListItem::new(line)
        })
        .collect();
    let mut state = ListState::default();
    state.select(if form.options.is_empty() {
        None
    } else {
        Some(form.selected)
    });
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
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
            ("/", "search"),
            ("r", "refresh"),
            ("o", "sort"),
            ("n", "new"),
            ("s", "start"),
            ("M", "all mine"),
            ("K", "done PRs"),
            ("q", "back"),
        ],
        Mode::Kanban => vec![
            ("h/l", "column"),
            ("⇧←/→", "reorder"),
            ("j/k", "card"),
            ("enter", "open"),
            ("e", "expand col"),
            ("m", "minimize col"),
            ("u", "filter users"),
            ("r", "refresh"),
            ("b/esc", "back"),
        ],
        Mode::KanbanFilter(ref form) => {
            if form.save_name.is_some() {
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
            }
        }
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
        Mode::ActiveStatusConfig(form) => {
            if form.adding.is_some() {
                vec![("type", "status"), ("enter", "add"), ("esc", "cancel")]
            } else {
                vec![
                    ("j/k", "move"),
                    ("i", "add"),
                    ("d×2", "remove"),
                    ("esc", "back"),
                ]
            }
        }
        Mode::Settings(form) => {
            if form.picker.is_some() {
                vec![
                    ("type", "filter"),
                    ("↑/↓", "move"),
                    ("enter", "pick"),
                    ("esc", "cancel"),
                ]
            } else {
                vec![("j/k", "move"), ("i/enter", "edit"), ("esc/q", "back")]
            }
        }
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
                    Some(t)
                        if crate::app::ticket_status_active(&t.status, &app.active_statuses) =>
                    {
                        "stop"
                    }
                    _ => "start",
                };
                let is_subtask = app
                    .detail
                    .as_ref()
                    .and_then(|t| t.issue_type.as_deref())
                    .map(|x| {
                        x.eq_ignore_ascii_case("sub-task") || x.eq_ignore_ascii_case("subtask")
                    })
                    .unwrap_or(false);
                let mut v: Vec<Hint> = vec![
                    ("tab", "pane"),
                    ("e", "edit"),
                    ("t", "trans"),
                    ("w", "time"),
                    ("i", "prio"),
                ];
                v.push(("O", "options"));
                if !is_subtask {
                    v.push(("T", "subtask"));
                }
                v.push(("@", "assign"));
                v.push(("R", "reviewer"));
                let has_pr = crate::app::ticket_has_pr(app);
                let devqa_started = crate::app::ticket_devqa_in_progress(app);
                if !has_pr {
                    v.push(("P", "open PR"));
                } else if devqa_started {
                    // DevQA already started → P resolves it (pass + 🚀).
                    v.push(("P", "pass DevQA"));
                }
                if !crate::app::detail_ticket_assigned_to_me(app) {
                    v.push((
                        "Q",
                        if devqa_started {
                            "re-open DevQA"
                        } else {
                            "begin DevQA"
                        },
                    ));
                }
                v.push(("K", "mark review"));
                v.push(("C", "ask ai"));
                v.push(("s", s_label));
                v.push(("D", "archive"));
                v.push(("esc", "back"));
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
                        ("C", "ask ai"),
                        ("esc", "back"),
                    ]
                } else {
                    vec![
                        ("tab", "next pane"),
                        ("j/k", "move"),
                        ("a", "add"),
                        ("d", "unlink"),
                        ("C", "ask ai"),
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
                ("C", "ask ai"),
                ("esc", "back"),
            ],
            DetailFocus::PrComments => vec![
                ("tab", "next pane"),
                ("j/k", "move"),
                ("r", "reply"),
                ("R", "resolve thread"),
                (
                    "H",
                    if app.show_resolved_pr_comments {
                        "hide resolved"
                    } else {
                        "show resolved"
                    },
                ),
                ("c", "ask ai"),
                ("esc", "back"),
            ],
            DetailFocus::Comments => vec![
                ("tab", "next pane"),
                ("j/k", "move"),
                ("c", "new"),
                ("R", "reply"),
                ("d", "delete (own)"),
                ("C", "ask ai"),
                ("esc", "back"),
            ],
        },
        Mode::TicketProjects(_) => {
            vec![("j/k", "move"), ("space/enter", "toggle"), ("esc", "back")]
        }
        Mode::Create(_) => vec![
            ("tab", "next"),
            ("enter", "next/submit"),
            ("F5/ctrl+enter/ctrl+s", "submit"),
            ("esc", "cancel"),
        ],
        Mode::Edit(form) => {
            if form.suggestion.is_some() {
                vec![("y", "keep claude rewrite"), ("n/esc", "reject")]
            } else {
                vec![
                    ("tab", "switch field"),
                    ("enter", "save (sum) / newline (desc)"),
                    ("ctrl+e", "$EDITOR"),
                    ("ctrl+r", "claude tighten"),
                    ("ctrl+s/F5", "save"),
                    ("esc", "cancel"),
                ]
            }
        }
        Mode::Comment(_) => vec![("ctrl+s", "submit"), ("esc", "cancel")],
        Mode::Transition(_) => vec![("j/k", "move"), ("enter", "submit"), ("esc", "cancel")],
        Mode::EditTime(_) => vec![("tab", "switch"), ("enter", "submit"), ("esc", "cancel")],
        Mode::EditPriority(_) => vec![("j/k", "move"), ("enter", "set"), ("esc", "cancel")],
        Mode::StartWorkPrompt(_) => vec![
            ("tab", "switch"),
            ("enter", "submit"),
            ("F5/ctrl+s", "submit"),
            ("esc", "cancel"),
        ],
        Mode::DevQaPrompt(_) => vec![
            ("←/→/space", "toggle"),
            ("enter", "launch"),
            ("esc", "cancel"),
        ],
        Mode::DevQaResolveConfirm(_) => vec![("enter/y", "post pass + 🚀"), ("esc/n", "cancel")],
        Mode::DevQaCleanupConfirm(_) => {
            vec![("enter/y", "discard + remove"), ("esc/n", "keep worktree")]
        }
        Mode::TicketOptions(_) => vec![("j/k", "move"), ("enter", "select"), ("esc", "close")],
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
        Mode::ConfluencePages(form) => {
            if form.search_active {
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
            }
        }
        Mode::PageView(_) => vec![], // PageView draws its own footer
        Mode::Tree(form) => {
            let mut hints: Vec<Hint> = vec![
                ("j/k", "move"),
                ("o/Tab", "toggle"),
                ("O/C", "expand/collapse all"),
                ("/", "search"),
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
                ("K", "done PRs"),
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
        Mode::ArchiveConfirm(_) => vec![("y/enter", "confirm"), ("n/esc", "cancel")],
        Mode::PrCommentReply(_) => vec![
            ("type", "edit"),
            ("←/→/↑/↓", "caret"),
            ("F5/^S", "submit"),
            ("esc", "cancel"),
        ],
        Mode::PrCreate(form) => {
            if form.remote_pick.is_some() {
                return vec![("j/k", "move"), ("enter", "save + push"), ("esc", "cancel")];
            }
            if form.review_state == crate::app::PrReviewState::Reviewing {
                if app.pending_pr_review.is_some() {
                    vec![("…", "running /review"), ("esc", "cancel")]
                } else {
                    vec![
                        ("j/k · PgUp/PgDn", "scroll"),
                        ("y", "submit"),
                        ("f", "fix session"),
                        ("R", "re-run review"),
                        ("esc", "back to editing"),
                    ]
                }
            } else {
                vec![
                    ("tab", "field"),
                    ("type", "edit/search"),
                    ("←/→", "caret"),
                    ("↑/↓", "line/pick"),
                    ("^R/F6", "claude rewrite"),
                    ("F5/^S", "submit PR"),
                    ("esc", "cancel"),
                ]
            }
        }
        Mode::Rules(_) => vec![
            ("j/k", "move"),
            ("a", "add"),
            ("d", "delete (twice)"),
            ("t", "toggle on/off"),
            ("enter", "edit"),
            ("l", "log"),
            ("esc", "back"),
        ],
        Mode::RuleEdit(_) => vec![
            ("j/k", "move"),
            ("enter/e", "edit"),
            ("tab/space", "cycle"),
            ("{", "var picker"),
            ("a", "add cond/act"),
            ("d", "delete (twice)"),
            ("^S", "save"),
            ("esc", "cancel"),
        ],
        Mode::RuleLog(_) => vec![
            ("j/k", "move"),
            ("g/G", "top/bottom"),
            ("PgUp/PgDn", "page"),
            ("r", "refresh"),
            ("esc", "back"),
        ],
        Mode::Home(_) => vec![
            ("1-8", "open view"),
            ("tab", "menu / feed"),
            ("j/k", "move"),
            ("enter", "open"),
            ("r", "refresh feed"),
            ("q", "quit"),
        ],
    }
}

fn draw_pr_comment_reply(f: &mut Frame, app: &App) {
    use ratatui::layout::{Constraint, Direction, Layout};
    let Mode::PrCommentReply(form) = &app.mode else {
        return;
    };
    let total = f.area();
    let height = (total.height * 80 / 100)
        .max(18)
        .min(total.height.saturating_sub(2));
    let width = (total.width * 70 / 100)
        .max(60)
        .min(total.width.saturating_sub(2));
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

    let kind_label = match form.parent_kind.as_str() {
        "review" => " threaded review reply ",
        "review_wrapper" => " new PR comment (review wrapper) ",
        _ => " new PR comment ",
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Line::from(Span::styled(
            kind_label,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();

    // Parent header — show who you're replying to and the body so the
    // context is right above your draft.
    lines.push(Line::from(vec![
        Span::styled("  replying to ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            form.parent_author.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    let wrap_w = inner.width.saturating_sub(4) as usize;
    for ln in form.parent_body.lines().take(8) {
        for chunk in wrap_line(ln, wrap_w.max(20)) {
            lines.push(Line::from(vec![
                Span::styled("  │ ", Style::default().fg(Color::DarkGray)),
                Span::styled(chunk, Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
    if form.parent_body.lines().count() > 8 {
        lines.push(Line::from(Span::styled(
            "  │ … (parent truncated)",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    }
    lines.push(Line::from(""));

    // Body editor with caret.
    lines.push(Line::from(Span::styled(
        "  your reply:",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    {
        let body = form.body.as_str();
        let body_cursor = form.body_cursor.min(body.len());
        let body_lines: Vec<&str> = if body.is_empty() {
            vec![""]
        } else {
            let mut v: Vec<&str> = body.split('\n').collect();
            if v.is_empty() {
                v.push("");
            }
            v
        };
        let mut offset = 0usize;
        for ln in body_lines.iter() {
            let line_end = offset + ln.len();
            let mut spans = vec![Span::raw("  ")];
            if body_cursor >= offset && body_cursor <= line_end {
                let rel = body_cursor - offset;
                let (pre, post) = ln.split_at(rel.min(ln.len()));
                spans.push(Span::raw(pre.to_string()));
                spans.push(Span::styled("▏", Style::default().fg(Color::Cyan)));
                spans.push(Span::raw(post.to_string()));
            } else {
                spans.push(Span::raw((*ln).to_string()));
            }
            lines.push(Line::from(spans));
            offset = line_end + 1;
        }
    }
    lines.push(Line::from(""));

    if let Some(err) = &form.error {
        for ln in err.lines() {
            for chunk in wrap_line(ln, wrap_w.max(20)) {
                lines.push(Line::from(Span::styled(
                    format!("  {}", chunk),
                    Style::default().fg(Color::Red),
                )));
            }
        }
        lines.push(Line::from(""));
    }
    if form.busy {
        lines.push(Line::from(Span::styled(
            "  posting…",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "  Enter newline · F5 / Ctrl-S submit · Esc cancel",
            Style::default().fg(Color::DarkGray),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_pr_create(f: &mut Frame, app: &App) {
    use crate::app::PrCreateForm;
    use ratatui::layout::{Constraint, Direction, Layout};
    let Mode::PrCreate(form) = &app.mode else {
        return;
    };
    let total = f.area();
    let height = (total.height * 80 / 100)
        .max(20)
        .min(total.height.saturating_sub(2));
    let width = (total.width * 70 / 100)
        .max(60)
        .min(total.width.saturating_sub(2));
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

    let title = format!(" pr · {} ", form.key);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Line::from(Span::styled(
            title,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Remote-picker sub-modal preempts the main form.
    if let Some(p) = &form.remote_pick {
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  where should jui PUSH your branch?",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(Span::styled(
            "  (must be a fork you can write to — usually NOT `origin`)",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
        lines.push(Line::from(""));
        for (i, (name, url)) in p.items.iter().enumerate() {
            let prefix = if i == p.selected { "  ▶ " } else { "    " };
            let style = if i == p.selected {
                Style::default()
                    .bg(Color::Rgb(60, 60, 80))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            // Flag `origin` so the user notices when they're about to push
            // to what's almost certainly upstream (no write access in a fork
            // workflow). Doesn't block selection — some setups DO have
            // origin = fork.
            let warn_tag = if name == "origin" {
                Span::styled(
                    "  ← likely upstream (read-only)",
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw("")
            };
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), Style::default().fg(Color::DarkGray)),
                Span::styled(format!("{name:<12}"), style.fg(Color::Cyan)),
                Span::styled(url.clone(), style.fg(Color::DarkGray)),
                warn_tag,
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "  This controls WHERE the branch pushes. PR target remains upstream/develop.",
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(Span::styled(
            "  j/k move · enter save+push · esc cancel · (saved per project)",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }

    // Pending-handle sub-modal preempts the main form.
    if let Some(p) = &form.pending_handle {
        let lines = vec![
            Line::from(""),
            Line::from(vec![
                Span::raw("  No GitHub handle for "),
                Span::styled(
                    p.display_name.clone(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(""),
            Line::from(vec![
                Span::raw("  github handle: "),
                Span::styled("@", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    p.handle.clone(),
                    Style::default()
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("█", Style::default().fg(Color::Yellow)),
            ]),
            Line::from(""),
            Line::from(Span::styled(
                "  Saved to ~/.config/jui/users.toml — Enter to save and retry, Esc to cancel.",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )),
        ];
        f.render_widget(Paragraph::new(lines), inner);
        return;
    }

    let cur = form.field;
    let label = |i: u8, name: &str| -> Span<'static> {
        let style = if cur == i {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Span::styled(format!("{:<10}", name), style)
    };

    let mut lines: Vec<Line> = Vec::new();

    if !form.route_hint.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("route     ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                form.route_hint.clone(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        lines.push(Line::from(""));
    }

    // Title field — split around the cursor so the caret renders inline.
    let title_cursor = form.title_cursor.min(form.title.len());
    let (title_pre, title_post) = form.title.split_at(title_cursor);
    let title_style = if cur == 0 {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let mut title_spans = vec![
        label(0, "title"),
        Span::raw(" "),
        Span::styled(title_pre.to_string(), title_style),
    ];
    if cur == 0 {
        title_spans.push(Span::styled("▏", Style::default().fg(Color::Yellow)));
    }
    title_spans.push(Span::styled(title_post.to_string(), title_style));
    lines.push(Line::from(title_spans));
    lines.push(Line::from(""));

    // Body field — multi-line. Walk by byte offset so we can splice a caret
    // glyph in at the cursor regardless of which line it lands on.
    let mut body_label_spans = vec![
        label(1, "body"),
        Span::raw(" (Enter inserts newline · ^R for claude rewrite)"),
    ];
    if app.pending_pr_body_improve.is_some() {
        body_label_spans.push(Span::raw("  "));
        body_label_spans.push(Span::styled(
            app.spinner_glyph().to_string(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        body_label_spans.push(Span::styled(
            " asking claude…",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::ITALIC),
        ));
    }
    lines.push(Line::from(body_label_spans));
    {
        let body = form.body.as_str();
        let body_cursor = form.body_cursor.min(body.len());
        let body_lines: Vec<&str> = if body.is_empty() {
            vec![""]
        } else {
            // Preserve trailing empty line so a caret after a final '\n' has somewhere to go.
            let mut v: Vec<&str> = body.split('\n').collect();
            if v.is_empty() {
                v.push("");
            }
            v
        };
        let mut offset = 0usize;
        let body_w = (inner.width as usize).saturating_sub(4).max(1);
        for ln in body_lines.iter() {
            let line_end = offset + ln.len();
            let rendered = if cur == 1 && body_cursor >= offset && body_cursor <= line_end {
                let rel = body_cursor - offset;
                insert_caret(ln, rel.min(ln.len()))
            } else {
                (*ln).to_string()
            };
            if rendered.is_empty() {
                lines.push(Line::from("  "));
            } else {
                let chars: Vec<char> = rendered.chars().collect();
                for chunk in chars.chunks(body_w) {
                    let text: String = chunk.iter().collect();
                    lines.push(Line::from(vec![Span::raw("  "), Span::raw(text)]));
                }
            }
            offset = line_end + 1; // +1 for the consumed '\n'
        }
    }
    lines.push(Line::from(""));

    // Suggestion overlay — shows Claude's rewrite with accept/reject hint.
    if let Some(s) = &form.suggestion {
        lines.push(Line::from(Span::styled(
            "  ── claude rewrite (y accept · n reject) ─────────────────────",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )));
        for ln in s.lines().take(20) {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(ln.to_string(), Style::default().fg(Color::Cyan)),
            ]));
        }
        if s.lines().count() > 20 {
            lines.push(Line::from(Span::styled(
                "  … (truncated; accept to insert in full)",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            )));
        }
        lines.push(Line::from(""));
    }

    // Reviewer field
    let reviewer_text = match &form.reviewer {
        Some((name, _)) => name.clone(),
        None => form.reviewer_query.clone(),
    };
    lines.push(Line::from(vec![
        label(2, "reviewer"),
        Span::raw(" "),
        Span::styled(reviewer_text, Style::default()),
        if cur == 2 && form.reviewer.is_none() {
            Span::styled("█", Style::default().fg(Color::Yellow))
        } else {
            Span::raw("")
        },
    ]));
    if cur == 2 && form.reviewer.is_none() {
        if form.reviewer_results.is_empty() {
            lines.push(Line::from(Span::styled(
                "  type ≥ 2 chars to search Jira users",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for (i, (name, _)) in form.reviewer_results.iter().enumerate().take(6) {
                let style = if i == form.reviewer_picker_selected {
                    Style::default()
                        .bg(Color::Rgb(60, 60, 80))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let prefix = if i == form.reviewer_picker_selected {
                    "  ▶ "
                } else {
                    "    "
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_string(), Style::default().fg(Color::DarkGray)),
                    Span::styled(name.clone(), style),
                ]));
            }
        }
    }
    lines.push(Line::from(""));

    // DevQA field
    let devqa_text = match &form.devqa {
        Some((name, _)) => name.clone(),
        None => form.devqa_query.clone(),
    };
    lines.push(Line::from(vec![
        label(3, "devqa"),
        Span::raw(" "),
        Span::styled(devqa_text, Style::default()),
        if cur == 3 && form.devqa.is_none() {
            Span::styled("█", Style::default().fg(Color::Yellow))
        } else {
            Span::raw("")
        },
    ]));
    if cur == 3 && form.devqa.is_none() {
        if form.devqa_results.is_empty() {
            lines.push(Line::from(Span::styled(
                "  type ≥ 2 chars to search Jira users",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for (i, (name, _)) in form.devqa_results.iter().enumerate().take(6) {
                let style = if i == form.devqa_picker_selected {
                    Style::default()
                        .bg(Color::Rgb(60, 60, 80))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let prefix = if i == form.devqa_picker_selected {
                    "  ▶ "
                } else {
                    "    "
                };
                lines.push(Line::from(vec![
                    Span::styled(prefix.to_string(), Style::default().fg(Color::DarkGray)),
                    Span::styled(name.clone(), style),
                ]));
            }
        }
    }
    lines.push(Line::from(""));

    if let Some(err) = &form.error {
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
    }
    if form.busy {
        lines.push(Line::from(Span::styled(
            "  submitting…",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    } else if form.review_state == crate::app::PrReviewState::Reviewing {
        // Headline + hint live above the review pane; the pane itself is
        // drawn separately below so it can be scrolled independently.
        let pending = app.pending_pr_review.is_some();
        if pending {
            lines.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(
                    app.spinner_glyph().to_string(),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    " claude review running… (esc cancels)",
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        } else if form.review_output.is_some() {
            lines.push(Line::from(Span::styled(
                "  /review output (j/k or PgUp/PgDn to scroll):",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )));
        } else {
            lines.push(Line::from(Span::styled(
                "  no review output yet — press R to run",
                Style::default().fg(Color::DarkGray),
            )));
        }
        lines.push(Line::from(Span::styled(
            "  y submit · f fix session · R re-run review · esc back to editing",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "  Tab next field · F5 / Ctrl-S submits · Esc cancels",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let _ = PrCreateForm::FIELD_COUNT; // assert constant references compile

    // Reserve the bottom half of `inner` for the review pane while in
    // Reviewing so the output has room to breathe without colliding with
    // the form rows above.
    if form.review_state == crate::app::PrReviewState::Reviewing {
        let split = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length((lines.len() as u16).min(inner.height / 2)),
                Constraint::Min(3),
            ])
            .split(inner);
        f.render_widget(Paragraph::new(lines), split[0]);
        draw_pr_review_pane(f, split[1], form);
    } else {
        f.render_widget(Paragraph::new(lines), inner);
    }
}

fn draw_pr_review_pane(f: &mut Frame, area: Rect, form: &crate::app::PrCreateForm) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray))
        .title(Span::styled(
            " claude /review ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(md) = &form.review_output else {
        let placeholder = if form.review_scroll > 0 {
            "(empty)"
        } else {
            "(no output yet — press R to run /review)"
        };
        f.render_widget(
            Paragraph::new(Span::styled(
                placeholder,
                Style::default().fg(Color::DarkGray),
            )),
            inner,
        );
        return;
    };
    let p = Paragraph::new(md.clone())
        .wrap(Wrap { trim: false })
        .scroll((form.review_scroll as u16, 0));
    f.render_widget(p, inner);
}

/// Greedy whitespace-aware wrap for long error/status messages in modals.
fn wrap_line(s: &str, w: usize) -> Vec<String> {
    if w == 0 || s.len() <= w {
        return vec![s.to_string()];
    }
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
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn draw_archive_confirm(f: &mut Frame, app: &App) {
    use ratatui::layout::{Alignment, Constraint, Direction, Layout};
    let Mode::ArchiveConfirm(form) = &app.mode else {
        return;
    };
    let total = f.area();
    let has_error = form.error.is_some();
    let height = if has_error { 22u16 } else { 9u16 }.min(total.height.saturating_sub(2));
    let width = (total.width * 70 / 100)
        .max(60)
        .min(total.width.saturating_sub(2));
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
    let bg = if has_error {
        Color::Rgb(40, 16, 16)
    } else {
        Color::Rgb(20, 28, 32)
    };
    let border = if has_error { Color::Red } else { Color::Yellow };
    f.render_widget(
        Block::default()
            .style(Style::default().bg(bg))
            .borders(Borders::NONE),
        area,
    );
    let title = if has_error {
        " archive failed "
    } else {
        " archive ticket? "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border))
        .title(Line::from(vec![Span::styled(
            title,
            Style::default().fg(border).add_modifier(Modifier::BOLD),
        )]));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled(
                form.key.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
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
            Span::styled(
                "[ Enter / Esc ]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  dismiss"),
        ]));
    } else {
        lines.push(Line::from(Span::styled(
            "  Will transition the ticket to Won't Do / Cancelled / Closed / Done.",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "[ y / Enter ]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  archive    "),
            Span::styled(
                "[ n / Esc ]",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  cancel"),
        ]));
    }
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Left), inner);
}

fn draw_ticket_options(f: &mut Frame, app: &App) {
    use ratatui::layout::{Alignment, Constraint, Direction, Layout};
    let Mode::TicketOptions(form) = &app.mode else {
        return;
    };
    let total = f.area();
    let height = 16u16.min(total.height.saturating_sub(2));
    let width = 86u16.max(56).min(total.width.saturating_sub(2));
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
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Line::from(vec![
            Span::styled(
                format!(" ticket options — {} ", form.key),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "(enter select · esc close)",
                Style::default().fg(Color::DarkGray),
            ),
        ]));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines = vec![
        Line::from(vec![
            Span::styled("  Claude session:   ", Style::default().fg(Color::DarkGray)),
            Span::raw(form.claude_session.clone().unwrap_or_else(|| "—".into())),
        ]),
        Line::from(vec![
            Span::styled("  opencode session: ", Style::default().fg(Color::DarkGray)),
            Span::raw(form.opencode_session.clone().unwrap_or_else(|| "—".into())),
        ]),
        Line::from(""),
    ];
    for (i, action) in form.actions.iter().enumerate() {
        let selected = i == form.selected;
        let label = match action {
            TicketOptionAction::Time => "change time / log work",
            TicketOptionAction::Priority => "change priority",
            TicketOptionAction::Reviewer => "change reviewer",
            TicketOptionAction::DevQa => "change DevQA assignee",
            TicketOptionAction::ResetPullRequest => "close PR + reset for new PR",
            TicketOptionAction::ClearClaudeSession => "clear Claude session id",
            TicketOptionAction::ClearOpencodeSession => "clear opencode session id",
        };
        let style = if selected {
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(
                if selected { "  ▶ " } else { "    " },
                Style::default().fg(Color::Green),
            ),
            Span::styled(label.to_string(), style),
        ]));
    }
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Left), inner);
}

fn draw_assign_picker(f: &mut Frame, app: &App) {
    use ratatui::layout::{Constraint, Direction, Layout};
    let Mode::AssignPicker(form) = &app.mode else {
        return;
    };

    // Center a popup ~60% wide, ~16 rows tall.
    let total = f.area();
    let height = 16u16.min(total.height.saturating_sub(2));
    let width = (total.width * 60 / 100)
        .max(50)
        .min(total.width.saturating_sub(2));
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
        AssignPurpose::DevQa => format!(" set DevQA on {} ", form.key),
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(Line::from(vec![Span::styled(
            title_text,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )]));
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
        Span::styled(
            form.query.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("█", Style::default().fg(Color::Yellow)),
    ]);
    f.render_widget(Paragraph::new(query_line), chunks[0]);

    let hint = match form.purpose {
        AssignPurpose::Assignee => "type ≥ 2 chars · empty + enter = me",
        AssignPurpose::Reviewer => "type ≥ 2 chars · enter to set",
        AssignPurpose::DevQa => "type ≥ 2 chars · enter to set",
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        ))),
        chunks[1],
    );

    // Results
    let result_lines: Vec<Line> = if form.results.is_empty() {
        vec![Line::from(Span::styled(
            "  (no results)",
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        ))]
    } else {
        form.results
            .iter()
            .enumerate()
            .map(|(i, (name, _id))| {
                let style = if i == form.selected {
                    Style::default()
                        .bg(Color::Rgb(60, 60, 80))
                        .add_modifier(Modifier::BOLD)
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
    let legend = legend_lines(app);
    let total = f.area();
    let two_col = !legend.is_empty();
    // Size the popup to whichever column is taller.
    let rows = (hints.len().max(legend.len()) as u16).max(1) + 4;
    let height = rows.min(total.height.saturating_sub(2));
    let width_pct = if two_col { 80 } else { 70 };
    let width = (total.width * width_pct / 100)
        .max(40)
        .min(total.width.saturating_sub(2));
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
            Span::styled(
                " help ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
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

    let key_style = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let exp_style = Style::default();
    let max_key_w = hints.iter().map(|(k, _)| k.len()).max().unwrap_or(1);
    let key_lines: Vec<Line> = hints
        .iter()
        .map(|(k, v)| {
            let pad = " ".repeat(max_key_w.saturating_sub(k.len()));
            let desc = long_desc(k, v).unwrap_or(v);
            Line::from(vec![
                Span::raw("  "),
                Span::styled(k.to_string(), key_style),
                Span::raw(pad),
                Span::raw("  "),
                Span::styled(desc.to_string(), exp_style),
            ])
        })
        .collect();

    if two_col {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(45), Constraint::Min(1)])
            .split(inner);
        f.render_widget(
            Paragraph::new(key_lines).alignment(Alignment::Left),
            cols[0],
        );
        f.render_widget(Paragraph::new(legend).alignment(Alignment::Left), cols[1]);
    } else {
        f.render_widget(Paragraph::new(key_lines).alignment(Alignment::Left), inner);
    }
}

/// Legend column shown next to the keybindings on the help overlay. Populated
/// for views where the role badges + state labels appear (List, Tree).
/// Returns an empty vec to fall back to single-column layout in other modes.
fn legend_lines(app: &App) -> Vec<Line<'static>> {
    let in_list = matches!(app.mode, Mode::List);
    let in_tree = matches!(app.mode, Mode::Tree(_));
    if !in_list && !in_tree {
        return Vec::new();
    }
    let dim = Style::default().fg(Color::DarkGray);
    let header = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let body = Style::default();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let entry = |badge: &str, badge_style: Style, label: &str| {
        Line::from(vec![
            Span::raw("  "),
            Span::styled(badge.to_string(), badge_style),
            Span::raw("  "),
            Span::styled(label.to_string(), body),
        ])
    };

    lines.push(Line::from(Span::styled(
        " Role badges  (purple = Jira · blue = GitHub)",
        header,
    )));
    let (b, s) = role_badge(MentionRole::Assigned);
    lines.push(entry(b, s, "Assigned to you (Jira)"));
    let (b, s) = role_badge(MentionRole::Reviewer);
    lines.push(entry(b, s, "Reviewer (Jira reviewer field)"));
    let (b, s) = role_badge(MentionRole::Github);
    lines.push(entry(b, s, "Reviewer (GitHub PR — same letter, blue)"));
    let (b, s) = role_badge(MentionRole::Mentioned);
    lines.push(entry(b, s, "Mentioned (GitHub @-mention)"));
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(
        " PR review state (your tracker)",
        header,
    )));
    if let Some((b, s)) = pr_state_label(MentionRole::Github, PrUserState::Awaiting) {
        lines.push(entry(b.trim_end(), s, "Awaiting your review (default)"));
    }
    if let Some((b, s)) = pr_state_label(MentionRole::Github, PrUserState::Reviewing) {
        lines.push(entry(b.trim_end(), s, "Actively reviewing (auto on Q)"));
    }
    if let Some((b, s)) = pr_state_label(MentionRole::Github, PrUserState::Completed) {
        lines.push(entry(
            b.trim_end(),
            s,
            "Completed (auto on gh APPROVED, hidden default)",
        ));
    }
    lines.push(entry(
        "[PR]",
        Style::default()
            .fg(Color::Rgb(80, 160, 255))
            .add_modifier(Modifier::BOLD),
        "You have an open PR authored — sinks below not-yet-PR'd in Tree",
    ));
    lines.push(Line::from(""));

    lines.push(Line::from(Span::styled(" Issue type glyphs", header)));
    for (gl, name) in [
        ("⚡", "Epic"),
        ("✦", "Story"),
        ("☑", "Task"),
        ("✗", "Bug"),
        ("↳", "Sub-task"),
        ("▲", "Improvement"),
        ("✱", "Spike"),
    ] {
        let style = Style::default().fg(type_color(Some(name)));
        lines.push(entry(gl, style, name));
    }
    lines.push(Line::from(""));

    if in_list {
        lines.push(Line::from(Span::styled(" Sections", header)));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("Active", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("    your assigned work (top pane)", dim),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("Mentioned", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(" reviewer + GitHub + @-mentions (bottom)", dim),
        ]));
    } else if in_tree {
        lines.push(Line::from(Span::styled(" Tree node sources", header)));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled("solid", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("       a ticket from one of your queues", dim),
        ]));
        lines.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "italic dim",
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC),
            ),
            Span::styled("  ancestor pulled in for context only", dim),
        ]));
    }

    lines
}

/// Per-(key, footer_short) → simple description shown in the `?` help overlay.
/// Footer hints stay terse for the bottom bar; the overlay lifts `mode_hints`
/// entries through this table so each command says what it actually changes.
fn long_desc(key: &str, short: &str) -> Option<&'static str> {
    match (key, short) {
        // List view
        ("j/k", "move") => Some("Move the selected ticket up or down."),
        ("tab", "expand subtasks") => Some("Show or hide this ticket's subtasks."),
        ("S-tab", "toggle section") => Some("Switch between active tickets and review mentions."),
        ("enter", "open") => Some("Open the selected ticket details."),
        ("/", "search") => Some("Search/filter matching rows as you type."),
        ("r", "refresh") => Some("Reload the active list from Jira."),
        ("o", "sort") => Some("Cycle the active list sort order."),
        ("n", "new") => Some("Create a new top-level Jira ticket."),
        ("s", "start") => Some("Start work: status, branch/worktree, assistant."),
        ("s", "stop") => Some("Stop work and move the ticket back."),
        ("T", "tree") => Some("Open the parent/subtask tree view."),
        ("a", "archive") => Some("Show closed, done, or archived tickets."),
        ("b", "board") => Some("Open the Jira-status kanban board."),
        ("p", "projects") => Some("Add or remove local project roots."),
        ("W", "workflow") => Some("Choose statuses that count as active work."),
        ("f", "confluence") => Some("Browse cached/live Confluence pages."),
        ("q", "quit") => Some("quit jui"),

        // Kanban
        ("h/l", "column") => Some("move between columns"),
        ("j/k", "card") => Some("move between cards"),
        ("e", "expand col") => Some("expand selected column"),
        ("m", "minimize col") => Some("minimize selected column"),
        ("u", "filter users") => Some("filter by assignee"),
        ("b/esc", "back") => Some("back to list"),

        // KanbanFilter
        ("type", "search") => Some("type to search users"),
        ("space/enter", "toggle") => Some("toggle assignee selection"),
        ("ctrl+c", "clear all") => Some("clear all selected"),
        ("esc", "done") => Some("close filter, apply"),
        ("ctrl+s", "save team") => Some("save filter as team"),
        ("type", "team name") => Some("type team name"),
        ("enter", "save") => Some("save and close"),
        ("esc", "cancel") => Some("cancel without saving"),

        // Projects mode
        ("a", "add") => Some("add a project"),
        ("d", "remove") => Some("remove selected project"),
        ("type", "filter") => Some("type to filter repos"),

        // Detail · Info
        ("tab", "pane") => Some("cycle to next pane"),
        ("e", "edit") => Some("edit ticket summary"),
        ("c", "comment") => Some("add a comment"),
        ("t", "trans") => Some("transition ticket status"),
        ("w", "time") => Some("log time worked"),
        ("i", "prio") => Some("edit ticket priority"),
        ("O", "options") => Some("Open time, priority, reviewer, and AI sessions."),
        ("L", "link") => Some("link a local project"),
        ("T", "subtask") => Some("create child sub-task"),
        ("@", "assign") => Some("change ticket assignee"),
        ("R", "reviewer") => Some("set ticket reviewer"),
        ("Y", "DevQA") => Some("set ticket DevQA assignee"),
        ("P", "open PR") => Some("Push branch, open PR, update Jira status."),
        ("P", "pass DevQA") => Some("Post DevQA passed, react, advance Jira."),
        ("Q", "begin DevQA") => Some("Check out PR and launch assistant for QA."),
        ("Q", "re-open DevQA") => Some("Re-open the existing DevQA assistant."),
        ("C", "ask ai") => Some("Ask the configured assistant about this ticket."),
        ("c", "ask ai") => Some("Ask the configured assistant about this context."),
        ("D", "archive") => Some("archive this ticket"),
        ("K", "mark review") => Some("Cycle local review marker: To Review/Reviewing/Done."),
        ("K", "done PRs") => Some("Show or hide PRs marked Done locally."),
        ("esc", "back") => Some("back to previous view"),

        // Detail · Projects
        ("tab", "next pane") => Some("cycle to next pane"),
        ("y", "approve") => Some("approve suggested project"),
        ("d", "dismiss/unlink") => Some("dismiss / unlink project"),
        ("d", "unlink") => Some("unlink project from ticket"),

        // Detail · Subtasks
        ("a", "add subtask") => Some("create new sub-task"),
        ("A", "toggle archived") => Some("show/hide archived subtasks"),

        // Detail · Comments
        ("c", "new") => Some("Write a new Jira comment."),
        ("R", "reply") => Some("Reply to the selected comment."),
        ("d", "delete (own)") => Some("Delete your own selected comment."),

        // Tree
        ("o/Tab", "toggle") => Some("toggle node expand/collapse"),
        ("O/C", "expand/collapse all") => Some("expand or collapse all"),
        ("c", "create child") => Some("create child of selected"),
        ("v", "two-col") => Some("toggle two-column layout"),
        ("Enter", "detail") => Some("open node in detail"),

        // Confluence
        ("enter", "view page") => Some("open page in viewer"),
        ("l/→", "drill into children") => Some("drill into child pages"),
        ("h/←/esc", "back") => Some("back to parent"),

        // Page viewer
        ("j/k", "scroll") => Some("scroll page up/down"),
        ("n/N", "next/prev") => Some("next/previous match"),
        ("e", "edit") => Some("edit in $EDITOR"),
        ("S", "sync") => Some("sync edits via mark"),

        // Archive / PR / Assign confirms
        ("y/enter", "confirm") => Some("Confirm this irreversible action."),
        ("n/esc", "cancel") => Some("Cancel and leave everything unchanged."),
        ("F5/^S", "submit") => Some("Submit this form now."),
        ("type", "edit/search") => Some("Type to edit the field or search."),
        ("type", "search/edit") => Some("Type to search, then edit/save."),
        ("↑/↓", "pick") => Some("Move through picker choices."),
        ("↑/↓", "move") => Some("Move within this list."),
        ("enter", "select") => Some("Choose the highlighted item."),
        ("enter", "next/submit") => Some("Advance fields; submit at the end."),
        ("F5/ctrl+enter/ctrl+s", "submit") => Some("Submit from any field."),
        ("tab", "switch") => Some("Move to the next form field."),
        ("enter", "submit") => Some("Submit the current form."),
        ("F5/ctrl+s", "submit") => Some("Submit without leaving the field."),
        ("←/→/space", "toggle") => Some("Toggle the selected option."),
        ("enter", "launch") => Some("Start the checkout and assistant."),
        ("enter/y", "post pass + 🚀") => Some("Post DevQA pass and advance Jira."),
        ("enter/y", "discard + remove") => Some("Discard changes and remove worktree."),
        ("esc/n", "keep worktree") => Some("Keep dirty worktree in place."),

        _ => None,
    }
}

fn render_hints(hints: &[Hint]) -> Line<'static> {
    let key_style = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
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

/// Insert the caret glyph at byte offset `cursor` inside `s`. Caller is
/// responsible for landing `cursor` on a UTF-8 boundary (the edit_* helpers do).
fn insert_caret(s: &str, cursor: usize) -> String {
    let cur = cursor.min(s.len());
    let mut out = String::with_capacity(s.len() + 3);
    out.push_str(&s[..cur]);
    out.push('▏');
    out.push_str(&s[cur..]);
    out
}

fn field_line<'a>(label: &'a str, value: &'a str, active: bool) -> Line<'a> {
    let style = if active {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
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
        "epic" => (
            "⚡",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        "story" => (
            "✦",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        "task" => ("✓", Style::default().fg(Color::Cyan)),
        "sub-task" | "subtask" => (
            "↳",
            Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
        ),
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
    if s.chars().count() <= n {
        s.to_string()
    } else if n == 0 {
        String::new()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn draw_confluence_spaces(f: &mut Frame, area: Rect, app: &App) {
    let Mode::ConfluenceSpaces(form) = &app.mode else {
        return;
    };
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
        let p = Paragraph::new(Span::styled(
            "no spaces found",
            Style::default().fg(Color::DarkGray),
        ));
        f.render_widget(p, inner);
        return;
    }
    let items: Vec<ListItem> = form
        .spaces
        .iter()
        .map(|s| {
            let key_span = Span::styled(
                format!(" {:<12} ", s.key),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
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
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, inner, &mut state);
}

fn draw_confluence_pages(f: &mut Frame, area: Rect, app: &App) {
    let Mode::ConfluencePages(form) = &app.mode else {
        return;
    };
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
            Span::styled(
                "/",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" {}▏", form.search_query),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
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
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("▶ ");
            f.render_stateful_widget(list, chunks[1], &mut state);
        }
        return;
    }

    // Normal page list.
    if form.pages.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "no pages",
                Style::default().fg(Color::DarkGray),
            )),
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
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, inner, &mut state);
}

fn draw_page_view(f: &mut Frame, area: Rect, app: &mut App) {
    use ratatui::widgets::{Scrollbar, ScrollbarOrientation, ScrollbarState};
    use ratatui_image::StatefulImage;
    let Mode::PageView(form) = &mut app.mode else {
        return;
    };

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
        Span::styled(
            form.title.clone(),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
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
        .map(|i| {
            (
                i,
                match &form.lines[i] {
                    PageLine::Spans(s) => PageLine::Spans(s.clone()),
                    PageLine::Blank => PageLine::Blank,
                    PageLine::Image { id, row, height } => PageLine::Image {
                        id: *id,
                        row: *row,
                        height: *height,
                    },
                },
            )
        })
        .collect();

    for row_idx in 0..visible_count {
        let (global_idx, line) = &visible[row_idx];
        let y = content_area.y + row_idx as u16;
        if y >= content_area.y + content_area.height {
            break;
        }
        let line_area = Rect::new(content_area.x, y, content_area.width.saturating_sub(1), 1);
        let is_cursor = form.search_matches.get(form.search_cursor) == Some(global_idx);
        let is_match = !is_cursor && form.search_matches.binary_search(global_idx).is_ok();

        match line {
            PageLine::Spans(spans) => {
                let styled: Vec<Span<'static>> = if is_cursor {
                    spans
                        .iter()
                        .map(|s| Span::styled(s.content.clone(), s.style.bg(Color::Rgb(80, 60, 0))))
                        .collect()
                } else if is_match {
                    spans
                        .iter()
                        .map(|s| {
                            Span::styled(s.content.clone(), s.style.bg(Color::Rgb(40, 40, 40)))
                        })
                        .collect()
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
            content_area.y,
            1,
            content_area.height,
        );
        let mut sb_state = ScrollbarState::new(max_scroll).position(scroll);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None),
            sb_area,
            &mut sb_state,
        );
    }

    // Footer
    let footer_area = chunks[2];
    let block = Block::default().borders(Borders::TOP);
    let inner = block.inner(footer_area);
    f.render_widget(block, footer_area);

    if form.search_active {
        let match_info = if form.search_matches.is_empty() {
            if form.search_query.is_empty() {
                String::new()
            } else {
                "no matches".to_string()
            }
        } else {
            format!("{}/{}", form.search_cursor + 1, form.search_matches.len())
        };
        let search_line = Line::from(vec![
            Span::styled(
                "/ ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(form.search_query.clone(), Style::default()),
            Span::styled("█", Style::default().fg(Color::Yellow)),
            Span::styled(
                if match_info.is_empty() {
                    String::new()
                } else {
                    format!("  {}", match_info)
                },
                Style::default().fg(Color::DarkGray),
            ),
        ]);
        let hint_line = Line::from(vec![
            Span::styled(
                "n/N",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" next/prev  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "Esc",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" close", Style::default().fg(Color::DarkGray)),
        ]);
        f.render_widget(Paragraph::new(vec![search_line, hint_line]), inner);
    } else {
        let pct = if form.lines.is_empty() {
            100
        } else {
            (scroll * 100 / form.lines.len()).min(100)
        };
        let hints = Line::from(vec![
            Span::styled(
                "j/k",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" scroll  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "d/u",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ½pg  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "/",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" search  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "e",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" edit  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "S",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" sync  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                "q",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
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
        t if t.eq_ignore_ascii_case("sub-task") || t.eq_ignore_ascii_case("subtask") => {
            Color::DarkGray
        }
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
    tree_node_line_with_pr_state(node, selected, None)
}

fn tree_node_line_with_pr_state(
    node: &TreeNode,
    selected: bool,
    pr_state: Option<PrUserState>,
) -> Line<'static> {
    let indent = "  ".repeat(node.depth as usize);
    let arrow = if !node.children.is_empty() {
        if node.expanded {
            "▼ "
        } else {
            "▶ "
        }
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
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC)
    };
    let bg = if selected {
        Style::default().bg(Color::Rgb(60, 60, 80))
    } else {
        Style::default()
    };
    let summary = if node.summary.is_empty() {
        "—".to_string()
    } else {
        node.summary.clone()
    };
    let type_color_v = type_color(node.issue_type.as_deref());
    let label = type_label(node.issue_type.as_deref());
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled(
            format!("{indent}{arrow}"),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(format!("{glyph} "), Style::default().fg(type_color_v)),
        Span::styled(
            label.to_string(),
            Style::default()
                .fg(type_color_v)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", Style::default()),
    ];
    // Role badge (only on leaves the user actually owns; ancestors get blanks
    // so columns line up).
    let badge_width = "[X] ".len();
    if let Some(role) = node.role {
        let (badge, style) = role_badge(role);
        spans.push(Span::styled(badge.to_string(), style));
        spans.push(Span::raw(" "));
        if let Some((label, label_style)) = devqa_or_pr_label(role, &node.status, pr_state) {
            spans.push(Span::styled(label.to_string(), label_style));
        } else if node.has_my_open_pr {
            // No incoming-review label to show here, so reuse the slot for
            // the "you have an open PR" hint. Blue (GitHub-side) to match
            // the other GitHub-origin badges.
            spans.push(Span::styled(
                "[PR]   ".to_string(),
                Style::default()
                    .fg(Color::Rgb(80, 160, 255))
                    .add_modifier(Modifier::BOLD),
            ));
        }
    } else {
        spans.push(Span::raw(" ".repeat(badge_width)));
    }
    spans.push(Span::styled(node.key.clone(), key_style));
    spans.push(Span::styled("  ", Style::default()));
    spans.push(Span::styled(summary, summary_style));
    spans.push(Span::styled(
        format!("  [{}]", node.status),
        Style::default().fg(Color::DarkGray),
    ));
    Line::from(spans).style(bg)
}

fn draw_tree(f: &mut Frame, area: Rect, app: &App) {
    let Mode::Tree(form) = &app.mode else { return };
    if app.ticket_search_active || !app.ticket_search_query.is_empty() {
        draw_tree_single(f, area, form, app);
        return;
    }
    if form.two_column {
        draw_tree_two_column(f, area, form, app);
    } else {
        draw_tree_single(f, area, form, app);
    }
}

fn draw_tree_single(f: &mut Frame, area: Rect, form: &TreeForm, app: &App) {
    let visible = app.tree_search_visible(form);
    let title = if app.ticket_search_active || !app.ticket_search_query.is_empty() {
        format!(" tickets — tree (/) /{} ", app.ticket_search_query)
    } else {
        " tickets — tree (T) ".to_string()
    };
    let inner = Block::default()
        .borders(Borders::ALL)
        .title(title.as_str())
        .inner(area);
    f.render_widget(Block::default().borders(Borders::ALL).title(title), area);
    if visible.is_empty() {
        f.render_widget(
            Paragraph::new("no tickets").style(Style::default().fg(Color::DarkGray)),
            inner,
        );
        return;
    }
    let viewport_h = inner.height as usize;
    let selected = form.selected.min(visible.len().saturating_sub(1));
    let scroll = selected.saturating_sub(viewport_h.saturating_sub(1) / 2);
    let scroll = scroll.min(visible.len().saturating_sub(viewport_h).max(0));
    let end = (scroll + viewport_h).min(visible.len());
    let lines: Vec<Line> = (scroll..end)
        .map(|i| {
            let node_idx = visible[i];
            let node = &form.nodes[node_idx];
            let pr_state = node.role.and_then(|_| Some(app.pr_state(&node.key)));
            tree_node_line_with_pr_state(node, i == selected, pr_state)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_tree_two_column(f: &mut Frame, area: Rect, form: &TreeForm, app: &App) {
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
            let Some(p_idx) = form.nodes.iter().position(|n| n.key == pk) else {
                break;
            };
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
                Style::default()
                    .bg(Color::Rgb(60, 60, 80))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(vec![
                Span::styled(
                    format!("{} ", type_glyph(n.issue_type.as_deref())),
                    Style::default().fg(type_color(n.issue_type.as_deref())),
                ),
                Span::styled(
                    n.key.clone(),
                    Style::default()
                        .fg(type_color(n.issue_type.as_deref()))
                        .add_modifier(Modifier::BOLD),
                ),
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

    let Some(&root_idx) = form.roots.get(selected_root_idx) else {
        return;
    };
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
            let node = &form.nodes[node_idx];
            let pr_state = node.role.and_then(|_| Some(app.pr_state(&node.key)));
            tree_node_line_with_pr_state(node, is_sel, pr_state)
        })
        .collect();
    f.render_widget(Paragraph::new(lines), right_inner);
}

fn push_subtree(nodes: &[TreeNode], idx: usize, out: &mut Vec<usize>) {
    let mut visited: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut stack: Vec<usize> = vec![idx];
    while let Some(i) = stack.pop() {
        if !visited.insert(i) {
            continue;
        }
        out.push(i);
        if nodes[i].expanded {
            // push children in reverse so visual order is preserved when popping
            for &c in nodes[i].children.iter().rev() {
                if !visited.contains(&c) {
                    stack.push(c);
                }
            }
        }
    }
}
