//! UI rendering for the TUI.

use super::App;
use super::app::ExchangeRow;
use super::app::FocusPane;
use super::app::RequestDetail;
use super::app::RequestTab;
use super::app::ResourceType;
use super::app::ResponseDetail;
use super::app::ResponseTab;
use super::app::TimingDetail;
use bytes::Bytes;
use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::layout::Direction;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::text::Text;
use ratatui::widgets::Block;
use ratatui::widgets::Borders;
use ratatui::widgets::Cell;
use ratatui::widgets::HighlightSpacing;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Row;
use ratatui::widgets::Scrollbar;
use ratatui::widgets::ScrollbarOrientation;
use ratatui::widgets::ScrollbarState;
use ratatui::widgets::Table;
use ratatui::widgets::Tabs;
use ratatui::widgets::Wrap;
use url::Url;

/// Draw the UI.
pub fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let area = frame.area();

    // Split the area into main content and help bar
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);

    if app.show_detail {
        // Split main area into table (top) and detail panes (bottom)
        let content_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(main_chunks[0]);

        draw_table(frame, app, content_chunks[0], app.focus == FocusPane::Table);

        // Split bottom into request (left) and response (right) panes
        let detail_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(content_chunks[1]);

        draw_request_pane(frame, app, detail_chunks[0]);
        draw_response_pane(frame, app, detail_chunks[1]);
    } else {
        draw_table(frame, app, main_chunks[0], true);
    }

    let help = create_help_bar(app);
    frame.render_widget(help, main_chunks[1]);
}

/// Draw the exchange table with optional filter input.
fn draw_table(frame: &mut Frame<'_>, app: &mut App, area: Rect, focused: bool) {
    // If filter is active or has text, show filter input above table
    let (filter_area, table_area) = if app.filter_active || app.has_filter() {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);
        (Some(chunks[0]), chunks[1])
    } else {
        (None, area)
    };

    if let Some(filter_rect) = filter_area {
        let filter_style = if app.filter_active {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let filter_text = if app.filter_active {
            format!("{}|", app.filter_text)
        } else {
            app.filter_text.clone()
        };

        let match_count = if app.has_filter() {
            format!(" ({}/{})", app.visible_count(), app.exchanges.len())
        } else {
            String::new()
        };

        let filter = Paragraph::new(filter_text)
            .style(Style::default().fg(Color::White))
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(filter_style)
                    .title(format!(" Filter{match_count} "))
                    .title_style(filter_style),
            );
        frame.render_widget(filter, filter_rect);
    }

    let visible_count = app.visible_count();

    // Create table directly from app data (inline to avoid lifetime issues)
    let table = create_exchange_table_inline(app, focused);
    frame.render_stateful_widget(table, table_area, &mut app.table_state);

    // Calculate visible rows (table height minus borders and header)
    let visible_rows = table_area.height.saturating_sub(3) as usize;

    if visible_count > visible_rows {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);

        let scrollbar_area = table_area.inner(ratatui::layout::Margin {
            vertical: 1,

            horizontal: 0,
        });

        frame.render_stateful_widget(scrollbar, scrollbar_area, &mut app.scrollbar_state);
    }
}

/// Draw the request pane with tabs.
fn draw_request_pane(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::Request;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    let exchange = app.detail_index.and_then(|idx| app.exchanges.get(idx));

    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let tabs = create_request_tabs(app.request_tab, focused);
    frame.render_widget(tabs, chunks[0]);

    let content_block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
        .border_style(border_style);

    let content_area = chunks[1];

    if let Some(ex) = exchange {
        let lines = match app.request_tab {
            RequestTab::General => create_request_general_content(&ex.request),
            RequestTab::Headers => create_request_headers_content(&ex.request),
            RequestTab::Query => create_query_content(&ex.request.url),
            RequestTab::Body => {
                create_body_content(&ex.request.body, ex.request.body_len, "Request")
            }
        };

        let inner_width = content_area.width.saturating_sub(2) as usize;
        let wrapped_line_count = calculate_wrapped_line_count(&lines, inner_width);

        let paragraph = Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((app.request_scroll, 0))
            .block(content_block);
        frame.render_widget(paragraph, content_area);

        // Calculate max scroll and render scrollbar
        let visible_height = content_area.height.saturating_sub(2) as usize;
        app.request_max_scroll = wrapped_line_count.saturating_sub(visible_height) as u16;

        if wrapped_line_count > visible_height {
            let max_scroll = app.request_max_scroll as usize;
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None);
            let mut scrollbar_state =
                ScrollbarState::new(max_scroll).position(app.request_scroll as usize);
            let scrollbar_area = content_area.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            });
            frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
        }
    } else {
        let empty = Paragraph::new("No selection").block(content_block);
        frame.render_widget(empty, content_area);
        app.request_max_scroll = 0;
    }
}

/// Draw the response pane with tabs.
fn draw_response_pane(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let focused = app.focus == FocusPane::Response;

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(0)])
        .split(area);

    let exchange = app.detail_index.and_then(|idx| app.exchanges.get(idx));

    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let tabs = create_response_tabs(app.response_tab, focused);
    frame.render_widget(tabs, chunks[0]);

    let content_block = Block::default()
        .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
        .border_style(border_style);

    let content_area = chunks[1];

    if let Some(ex) = exchange {
        let lines = match app.response_tab {
            ResponseTab::General => {
                create_response_general_content(ex.response.as_ref(), ex.resource_type)
            }
            ResponseTab::Headers => create_response_headers_content(ex.response.as_ref()),
            ResponseTab::Body => {
                if let Some(resp) = &ex.response {
                    create_body_content(&resp.body, resp.body_len, "Response")
                } else {
                    vec![Line::from(Span::styled(
                        "(pending)",
                        Style::default().fg(Color::DarkGray),
                    ))]
                }
            }
            ResponseTab::Cookies => create_cookies_content(ex.response.as_ref()),
            ResponseTab::Timings => create_timings_content(ex.timings.as_ref()),
        };

        let inner_width = content_area.width.saturating_sub(2) as usize;
        let wrapped_line_count = calculate_wrapped_line_count(&lines, inner_width);

        let paragraph = Paragraph::new(Text::from(lines))
            .wrap(Wrap { trim: false })
            .scroll((app.response_scroll, 0))
            .block(content_block);
        frame.render_widget(paragraph, content_area);

        // Calculate max scroll and render scrollbar
        let visible_height = content_area.height.saturating_sub(2) as usize;
        app.response_max_scroll = wrapped_line_count.saturating_sub(visible_height) as u16;

        if wrapped_line_count > visible_height {
            let max_scroll = app.response_max_scroll as usize;
            let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None);
            let mut scrollbar_state =
                ScrollbarState::new(max_scroll).position(app.response_scroll as usize);
            let scrollbar_area = content_area.inner(ratatui::layout::Margin {
                vertical: 1,
                horizontal: 0,
            });
            frame.render_stateful_widget(scrollbar, scrollbar_area, &mut scrollbar_state);
        }
    } else {
        let empty = Paragraph::new("No selection").block(content_block);
        frame.render_widget(empty, content_area);
        app.response_max_scroll = 0;
    }
}

/// Create request general content (method, URL, and version).
fn create_request_general_content(request: &RequestDetail) -> Vec<Line<'static>> {
    let method_style = match request.method.as_str() {
        "GET" => Style::default().fg(Color::Green),
        "POST" => Style::default().fg(Color::Blue),
        "PUT" => Style::default().fg(Color::Yellow),
        "DELETE" => Style::default().fg(Color::Red),
        "PATCH" => Style::default().fg(Color::Magenta),
        _ => Style::default(),
    };

    vec![
        Line::from(vec![
            Span::styled("Request Method: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                request.method.clone(),
                method_style.add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("HTTP Version: ", Style::default().fg(Color::Yellow)),
            Span::raw(request.version.clone()),
        ]),
        Line::from(vec![
            Span::styled("Request URL: ", Style::default().fg(Color::Yellow)),
            Span::raw(request.url.clone()),
        ]),
    ]
}

/// Create response general content (status code, version, remote address, and
/// resource type).
fn create_response_general_content(
    response: Option<&ResponseDetail>,

    resource_type: ResourceType,
) -> Vec<Line<'static>> {
    let Some(resp) = response else {
        return vec![Line::from(Span::styled(
            "(pending)",
            Style::default().fg(Color::DarkGray),
        ))];
    };

    let status_color = match resp.status {
        s if s >= 500 => Color::Red,
        s if s >= 400 => Color::Yellow,
        s if s >= 300 => Color::Cyan,
        s if s >= 200 => Color::Green,
        _ => Color::White,
    };

    let type_style = match resource_type {
        ResourceType::Document => Style::default().fg(Color::Cyan),
        ResourceType::Stylesheet => Style::default().fg(Color::Magenta),
        ResourceType::Script => Style::default().fg(Color::Yellow),
        ResourceType::Image => Style::default().fg(Color::Green),
        ResourceType::Json => Style::default().fg(Color::Blue),
        _ => Style::default().fg(Color::DarkGray),
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("Status Code: ", Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{} {}", resp.status, resp.reason),
                Style::default()
                    .fg(status_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled("HTTP Version: ", Style::default().fg(Color::Yellow)),
            Span::raw(resp.version.clone()),
        ]),
    ];

    if let Some(addr) = &resp.remote_addr {
        lines.push(Line::from(vec![
            Span::styled("Remote Address: ", Style::default().fg(Color::Yellow)),
            Span::raw(addr.clone()),
        ]));
    }

    lines.push(Line::from(vec![
        Span::styled("Resource Type: ", Style::default().fg(Color::Yellow)),
        Span::styled(resource_type.full_name(), type_style),
    ]));

    lines
}

/// Create request headers content.
fn create_request_headers_content(request: &RequestDetail) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    if request.headers.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no headers)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (name, value) in &request.headers {
            lines.push(Line::from(vec![
                Span::styled(format!("{name}: "), Style::default().fg(Color::Yellow)),
                Span::raw(value.clone()),
            ]));
        }
    }

    lines
}

/// Create response headers content.
fn create_response_headers_content(response: Option<&ResponseDetail>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let Some(resp) = response else {
        lines.push(Line::from(Span::styled(
            "(pending)",
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    };

    if resp.headers.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no headers)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (name, value) in &resp.headers {
            lines.push(Line::from(vec![
                Span::styled(format!("{name}: "), Style::default().fg(Color::Yellow)),
                Span::raw(value.clone()),
            ]));
        }
    }

    lines
}

/// Create query parameters content.
fn create_query_content(url: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let params: Vec<(String, String)> = Url::parse(url)
        .map(|u| {
            u.query_pairs()
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect()
        })
        .unwrap_or_default();

    if params.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no query parameters)",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        for (key, value) in params {
            lines.push(Line::from(vec![
                Span::styled(format!("{key}: "), Style::default().fg(Color::Cyan)),
                Span::raw(value),
            ]));
        }
    }

    lines
}

/// Create body content.
/// Render a body for the detail pane.
///
/// `body` is what the row kept; `body_len` is how long the body actually
/// was. When they differ the pane says so, so a body the view cut is
/// never mistaken for one the server sent short. The export keeps the
/// whole thing either way.
fn create_body_content(body: &Bytes, body_len: usize, label: &str) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    if body_len == 0 {
        lines.push(Line::from(Span::styled(
            format!("({label} has no body)"),
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    }

    if let Ok(text) = std::str::from_utf8(body) {
        // Try to pretty-print JSON. A cut body is no longer valid JSON,
        // so this falls through to the plain-text path for those.
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(text)
            && let Ok(pretty) = serde_json::to_string_pretty(&json)
        {
            for line in pretty.lines() {
                lines.push(Line::from(line.to_owned()));
            }
        } else {
            for line in text.lines() {
                lines.push(Line::from(line.to_owned()));
            }
        }
    } else {
        lines.push(Line::from(Span::styled(
            format!("[Binary data: {body_len} bytes]"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    if body.len() < body_len {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!(
                "[showing the first {} of {} — the export keeps the whole body]",
                format_bytes(body.len()),
                format_bytes(body_len),
            ),
            Style::default().fg(Color::Yellow),
        )));
    }

    lines
}

/// Format a byte count for display: `931 B`, `12.4 KB`, `3.7 MB`.
fn format_bytes(bytes: usize) -> String {
    #[expect(
        clippy::cast_precision_loss,
        reason = "display only; sizes here are far below f64's exact-integer range"
    )]
    let value = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", value / 1024.0)
    } else {
        format!("{:.1} MB", value / (1024.0 * 1024.0))
    }
}

/// Create cookies content from Set-Cookie headers.
fn create_cookies_content(response: Option<&ResponseDetail>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let Some(resp) = response else {
        lines.push(Line::from(Span::styled(
            "(pending)",
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    };

    let cookies: Vec<_> = resp
        .headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("set-cookie"))
        .collect();

    if cookies.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no cookies)",
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    }

    for (_, value) in cookies {
        // Parse cookie: name=value; attributes...
        let parts: Vec<&str> = value.split(';').collect();
        if let Some(name_value) = parts.first()
            && let Some((name, val)) = name_value.split_once('=')
        {
            lines.push(Line::from(vec![Span::styled(
                name.trim().to_owned(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )]));
            lines.push(Line::from(vec![
                Span::styled("  Value: ", Style::default().fg(Color::DarkGray)),
                Span::raw(val.trim().to_owned()),
            ]));

            for attr in parts.iter().skip(1) {
                let attr = attr.trim();
                if let Some((attr_name, attr_val)) = attr.split_once('=') {
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("  {}: ", capitalize(attr_name.trim())),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::raw(attr_val.trim().to_owned()),
                    ]));
                } else if !attr.is_empty() {
                    lines.push(Line::from(vec![Span::styled(
                        format!("  {}", capitalize(attr)),
                        Style::default().fg(Color::DarkGray),
                    )]));
                }
            }
            lines.push(Line::from(""));
        }
    }

    lines
}

/// Capitalize first letter of a string.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().chain(chars).collect(),
    }
}

/// Create the timings content with a waterfall-style breakdown.
fn create_timings_content(timings: Option<&TimingDetail>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    let Some(t) = timings else {
        lines.push(Line::from(Span::styled(
            "No timing data available",
            Style::default().fg(Color::DarkGray),
        )));
        return lines;
    };

    if t.connection_reused {
        lines.push(Line::from(vec![
            Span::styled("Connection: ", Style::default().fg(Color::DarkGray)),
            Span::styled("reused", Style::default().fg(Color::Green)),
        ]));
    } else {
        lines.push(Line::from(vec![
            Span::styled("Connection: ", Style::default().fg(Color::DarkGray)),
            Span::raw("new"),
        ]));
    }
    lines.push(Line::from(""));

    let total = t.total_ms.unwrap_or(0) as f64;

    let format_timing_row =
        |name: &'static str, value: Option<f64>, color: Color| -> Vec<Line<'static>> {
            let mut row_lines = Vec::new();
            match value {
                Some(ms) if ms > 0.0 => {
                    // Calculate bar width (max 30 chars)
                    let bar_width = if total > 0.0 {
                        ((ms / total) * 30.0).round() as usize
                    } else {
                        0
                    };
                    let bar_width = bar_width.clamp(1, 30);
                    let bar = "|".repeat(bar_width);

                    row_lines.push(Line::from(vec![
                        Span::styled(format!("{name:<12}"), Style::default().fg(Color::DarkGray)),
                        Span::styled(bar, Style::default().fg(color)),
                        Span::raw(" "),
                        Span::styled(format_duration(ms), Style::default().fg(Color::White)),
                    ]));
                }
                Some(_) => {
                    // Zero value
                    row_lines.push(Line::from(vec![
                        Span::styled(format!("{name:<12}"), Style::default().fg(Color::DarkGray)),
                        Span::styled("0 ms", Style::default().fg(Color::DarkGray)),
                    ]));
                }
                None => {
                    // N/A
                    row_lines.push(Line::from(vec![
                        Span::styled(format!("{name:<12}"), Style::default().fg(Color::DarkGray)),
                        Span::styled("-", Style::default().fg(Color::DarkGray)),
                    ]));
                }
            }
            row_lines
        };

    lines.extend(format_timing_row("Blocked", t.blocked_ms, Color::Gray));

    lines.extend(format_timing_row("Connect", t.connect_ms, Color::Yellow));

    lines.extend(format_timing_row("SSL", t.ssl_ms, Color::Magenta));

    lines.extend(format_timing_row("Send", t.send_ms, Color::Blue));

    lines.extend(format_timing_row("Wait (TTFB)", t.wait_ms, Color::Green));

    lines.extend(format_timing_row("Receive", t.receive_ms, Color::Cyan));

    lines.push(Line::from(""));

    lines.push(Line::from(vec![
        Span::styled(
            "Total       ",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format_duration(total),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]));

    lines
}

/// Format a duration in milliseconds for display.
fn format_duration(ms: f64) -> String {
    if ms >= 1000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else if ms >= 1.0 {
        format!("{ms:.2} ms")
    } else {
        format!("{ms:.3} ms")
    }
}

/// Create tabs widget for the request pane.
fn create_request_tabs(selected: RequestTab, focused: bool) -> Tabs<'static> {
    let tab_titles: Vec<Line<'_>> = RequestTab::all()
        .iter()
        .map(|t| Line::from(format!(" {t} ")))
        .collect();

    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    Tabs::new(tab_titles)
        .block(
            Block::default()
                .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                .border_style(border_style)
                .title(" Request ")
                .title_style(title_style),
        )
        .select(selected.index())
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .divider(Span::raw(" | "))
}

/// Create tabs widget for the response pane.
fn create_response_tabs(selected: ResponseTab, focused: bool) -> Tabs<'static> {
    let tab_titles: Vec<Line<'_>> = ResponseTab::all()
        .iter()
        .map(|t| Line::from(format!(" {t} ")))
        .collect();

    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    Tabs::new(tab_titles)
        .block(
            Block::default()
                .borders(Borders::TOP | Borders::LEFT | Borders::RIGHT)
                .border_style(border_style)
                .title(" Response ")
                .title_style(title_style),
        )
        .select(selected.index())
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )
        .divider(Span::raw(" | "))
}

/// Calculate the total number of lines after word wrapping.
fn calculate_wrapped_line_count(lines: &[Line<'_>], width: usize) -> usize {
    if width == 0 {
        return lines.len();
    }
    lines
        .iter()
        .map(|line| {
            let line_width: usize = line.spans.iter().map(|s| s.content.len()).sum();
            if line_width == 0 {
                1 // Empty lines still take 1 line
            } else {
                line_width.div_ceil(width)
            }
        })
        .sum()
}

/// Create the exchange table widget.
#[allow(dead_code)]
fn create_exchange_table(exchanges: &[ExchangeRow], focused: bool) -> Table<'static> {
    let visible: Vec<(usize, &ExchangeRow)> = exchanges.iter().enumerate().collect();
    create_exchange_table_from_slice(&visible, focused)
}

/// Create the exchange table widget directly from app state.
fn create_exchange_table_inline(app: &App, focused: bool) -> Table<'static> {
    let visible = app.visible_exchanges();
    create_exchange_table_from_slice(&visible, focused)
}

/// Create the exchange table widget from a slice of visible exchanges.
fn create_exchange_table_from_slice(
    visible: &[(usize, &ExchangeRow)],
    focused: bool,
) -> Table<'static> {
    let header = ["#", "Method", "URL", "Type", "Status", "Size", "Time"]
        .into_iter()
        .map(|h| Cell::from(h).style(Style::default().add_modifier(Modifier::BOLD)))
        .collect::<Row<'_>>()
        .height(1)
        .style(Style::default().fg(Color::Yellow));

    let rows: Vec<Row<'static>> = visible
        .iter()
        .map(|(original_index, exchange)| {
            let status_style = match exchange.status {
                Some(s) if s >= 500 => Style::default().fg(Color::Red),
                Some(s) if s >= 400 => Style::default().fg(Color::Yellow),
                Some(s) if s >= 300 => Style::default().fg(Color::Cyan),
                Some(s) if s >= 200 => Style::default().fg(Color::Green),
                Some(_) => Style::default(),
                None => Style::default().fg(Color::DarkGray),
            };

            let method_style = match exchange.method.as_str() {
                "GET" => Style::default().fg(Color::Green),
                "POST" => Style::default().fg(Color::Blue),
                "PUT" => Style::default().fg(Color::Yellow),
                "DELETE" => Style::default().fg(Color::Red),
                "PATCH" => Style::default().fg(Color::Magenta),
                _ => Style::default(),
            };

            let type_style = match exchange.resource_type {
                ResourceType::Document => Style::default().fg(Color::Cyan),
                ResourceType::Stylesheet => Style::default().fg(Color::Magenta),
                ResourceType::Script => Style::default().fg(Color::Yellow),
                ResourceType::Image => Style::default().fg(Color::Green),
                ResourceType::Json => Style::default().fg(Color::Blue),
                _ => Style::default().fg(Color::DarkGray),
            };

            // Display 1-based original row number
            let display_id = original_index + 1;

            Row::new(vec![
                Cell::from(Text::from(display_id.to_string())),
                Cell::from(Text::from(exchange.method.clone())).style(method_style),
                Cell::from(Text::from(exchange.url.clone())),
                Cell::from(Text::from(exchange.type_display())).style(type_style),
                Cell::from(Text::from(exchange.status_display())).style(status_style),
                Cell::from(Text::from(exchange.size_display())),
                Cell::from(Text::from(exchange.time_display())),
            ])
            .height(1)
        })
        .collect();

    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let (highlight_style, highlight_symbol) = if focused {
        // Focused: reversed (inverted colors) with >>
        (Style::default().add_modifier(Modifier::REVERSED), ">> ")
    } else {
        // Unfocused: cyan text with bullet to show which row is selected
        (Style::default().fg(Color::Cyan), " . ")
    };

    Table::new(
        rows,
        [
            Constraint::Length(5), // ID
            Constraint::Length(7), // Method
            Constraint::Min(20),   // URL (flexible)
            Constraint::Length(6), // Type
            Constraint::Length(7), // Status
            Constraint::Length(9), // Size
            Constraint::Length(9), // Time
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(" Traffic ")
            .title_style(if focused {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            }),
    )
    .row_highlight_style(highlight_style)
    .highlight_symbol(highlight_symbol)
    .highlight_spacing(HighlightSpacing::Always)
}

/// Create the help bar at the bottom.
fn create_help_bar(app: &App) -> Paragraph<'static> {
    let key_style = Style::default()
        .fg(Color::Black)
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let desc_style = Style::default().fg(Color::DarkGray);
    let sep_style = Style::default().fg(Color::DarkGray);

    // Filter input mode
    if app.filter_active {
        let spans = vec![
            Span::styled(" Enter ", key_style),
            Span::styled(" Confirm ", desc_style),
            Span::styled("|", sep_style),
            Span::styled(" Esc ", key_style),
            Span::styled(" Cancel ", desc_style),
            Span::styled("|", sep_style),
            Span::styled(" Type to filter URLs ", desc_style),
        ];
        return Paragraph::new(Line::from(spans)).centered();
    }

    let spans = match (app.show_detail, app.focus) {
        (true, FocusPane::Request | FocusPane::Response) => {
            // Detail pane focused
            vec![
                Span::styled(" q ", key_style),
                Span::styled(" Quit ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" Esc ", key_style),
                Span::styled(" Close ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" Tab ", key_style),
                Span::styled(" Focus ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" h/l ", key_style),
                Span::styled(" Tabs ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" j/k ", key_style),
                Span::styled(" Scroll ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" / ", key_style),
                Span::styled(" Filter ", desc_style),
            ]
        }
        (true, FocusPane::Table) => {
            // Table focused with detail pane visible
            vec![
                Span::styled(" q ", key_style),
                Span::styled(" Quit ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" Esc ", key_style),
                Span::styled(" Close ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" Tab ", key_style),
                Span::styled(" Focus ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" j/k ", key_style),
                Span::styled(" Navigate ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" G ", key_style),
                Span::styled(" Bottom ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" Enter ", key_style),
                Span::styled(" Select ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" / ", key_style),
                Span::styled(" Filter ", desc_style),
            ]
        }
        (false, _) => {
            // Table only view
            vec![
                Span::styled(" q ", key_style),
                Span::styled(" Quit ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" j/k ", key_style),
                Span::styled(" Navigate ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" G ", key_style),
                Span::styled(" Bottom ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" Enter ", key_style),
                Span::styled(" Details ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" / ", key_style),
                Span::styled(" Filter ", desc_style),
                Span::styled("|", sep_style),
                Span::styled(" Del ", key_style),
                Span::styled(" Clear ", desc_style),
            ]
        }
    };

    Paragraph::new(Line::from(spans)).centered()
}
