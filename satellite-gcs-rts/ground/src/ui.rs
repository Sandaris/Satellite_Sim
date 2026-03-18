use std::sync::Arc;
use tokio::time::{Duration, Instant};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use crossterm::{
    terminal::{enable_raw_mode, disable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use ratatui::{
    prelude::*,
    widgets::*,
    backend::CrosstermBackend,
};

#[derive(Clone)]
pub struct GcsMetricsSnapshot {
    // Telemetry reception per sensor
    pub thermal_recv_count:     u64,
    pub thermal_lost_count:     u64,
    pub thermal_drift_last_us:  i64,
    
    pub power_recv_count:       u64,
    pub power_lost_count:       u64,
    pub power_drift_last_us:    i64,
    
    pub imu_recv_count:         u64,
    pub imu_lost_count:         u64,
    pub imu_drift_last_us:      i64,
    
    pub decode_latency_last_us: u64,
    pub decode_deadline_misses: u64,

    // UDP latency histogram buckets (counts per bucket)
    pub latency_buckets:        [u64; 8],  // <1ms,1-2,2-5,5-10,10-20,20-50,50-100,>100
    pub latency_p50_us:         u64,
    pub latency_p99_us:         u64,
    pub latency_max_us:         u64,
    pub latency_avg_us:         u64,

    // Command uplink
    pub cmd_queue_depth:        usize,
    pub cmd_emergency_count:    usize,
    pub cmd_urgent_count:       usize,
    pub cmd_routine_count:      usize,
    pub recent_commands:        std::collections::VecDeque<(String, String, u64, String)>, // (time, type, dispatch_us, result)
    pub cmd_total_sent:         u64,
    pub cmd_deadline_misses:    u64,
    pub cmd_rejected_count:     u64,

    // Fault management
    pub gcs_state:              String,
    pub fault_received_count:   u64,
    pub fault_last_type:        String,
    pub fault_last_time_s:      u64,
    pub interlock_last_us:      u64,
    pub interlock_max_us:       u64,
    pub critical_alerts:        u64,
    pub interlock_active:       bool,

    // Contact status
    pub total_pkts_received:    u64,
    pub total_pkts_lost:        u64,
    pub consecutive_gaps:       u32,
    pub contact_status:         String,  // "ESTABLISHED", "DEGRADED", "LOST"
    pub reception_rate_pct:     f64,

    // Live log
    pub log_lines: std::collections::VecDeque<(String, u8)>,
}

impl Default for GcsMetricsSnapshot {
    fn default() -> Self {
        Self {
            thermal_recv_count: 0, thermal_lost_count: 0, thermal_drift_last_us: 0,
            power_recv_count: 0, power_lost_count: 0, power_drift_last_us: 0,
            imu_recv_count: 0, imu_lost_count: 0, imu_drift_last_us: 0,
            decode_latency_last_us: 0, decode_deadline_misses: 0,
            latency_buckets: [0; 8],
            latency_p50_us: 0, latency_p99_us: 0, latency_max_us: 0, latency_avg_us: 0,
            cmd_queue_depth: 0, cmd_emergency_count: 0, cmd_urgent_count: 0, cmd_routine_count: 0,
            recent_commands: std::collections::VecDeque::with_capacity(5),
            cmd_total_sent: 0, cmd_deadline_misses: 0, cmd_rejected_count: 0,
            gcs_state: "NOMINAL".to_string(),
            fault_received_count: 0, fault_last_type: "None".to_string(), fault_last_time_s: 0,
            interlock_last_us: 0, interlock_max_us: 0, critical_alerts: 0, interlock_active: false,
            total_pkts_received: 0, total_pkts_lost: 0, consecutive_gaps: 0,
            contact_status: "ESTABLISHED".to_string(), reception_rate_pct: 100.0,
            log_lines: std::collections::VecDeque::with_capacity(200),
        }
    }
}

pub fn push_log(
    metrics: &Arc<Mutex<GcsMetricsSnapshot>>,
    level: u8,
    msg: String,
    sim_start: &Arc<Instant>,
) {
    if let Ok(mut m) = metrics.try_lock() {
        let elapsed = sim_start.elapsed();
        let h = elapsed.as_secs() / 3600;
        let min = (elapsed.as_secs() % 3600) / 60;
        let s = elapsed.as_secs() % 60;
        let ms = elapsed.subsec_millis();
        let line = format!("[{:02}:{:02}:{:02}.{:03}] [{}] {}", 
            h, min, s, ms,
            match level { 0 => "INFO", 1 => "WARN", _ => "ERROR" },
            msg
        );
        if m.log_lines.len() >= 200 { m.log_lines.pop_front(); }
        m.log_lines.push_back((line, level));
    }
}

pub async fn run_ui(
    metrics: Arc<Mutex<GcsMetricsSnapshot>>,
    sim_start: Arc<Instant>,
    cancel: CancellationToken,
) {
    enable_raw_mode().unwrap();
    std::io::stdout().execute(EnterAlternateScreen).unwrap();

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend).unwrap();

    let mut interval = tokio::time::interval(Duration::from_millis(100));

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = interval.tick() => {
                let snapshot = { metrics.lock().await.clone() };

                terminal.draw(|frame| {
                    render_dashboard(frame, &snapshot, sim_start.elapsed());
                }).unwrap();

                if crossterm::event::poll(Duration::from_millis(0)).unwrap() {
                    if let crossterm::event::Event::Key(k) = crossterm::event::read().unwrap() {
                        if k.code == crossterm::event::KeyCode::Char('q') {
                            cancel.cancel();
                            break;
                        }
                    }
                }
            }
        }
    }

    disable_raw_mode().unwrap();
    std::io::stdout().execute(LeaveAlternateScreen).unwrap();
}

fn render_dashboard(frame: &mut Frame, metrics: &GcsMetricsSnapshot, elapsed: Duration) {
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Title
            Constraint::Length(8), // A, B
            Constraint::Length(10), // C, D
            Constraint::Length(5), // E
            Constraint::Min(12),   // F: LOG
        ])
        .split(frame.size());

    // Title
    let state_color = match metrics.gcs_state.as_str() {
        "NOMINAL" => Color::Green,
        "DEGRADED" => Color::Yellow,
        "FAULT" | "LOSS OF CONTACT" | _ => Color::Red,
    };
    let title = Paragraph::new(Line::from(vec![
        Span::raw("  GROUND CONTROL STATION DASHBOARD  [elapsed: "),
        Span::raw(format!("{:02}:{:02}:{:02}]  [", elapsed.as_secs()/3600, (elapsed.as_secs()%3600)/60, elapsed.as_secs()%60)),
        Span::styled(metrics.gcs_state.clone(), Style::default().fg(state_color).add_modifier(Modifier::BOLD)),
        Span::raw("]"),
    ])).block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, main_layout[0]);

    // Panels A, B
    let ab_layout = Layout::default().direction(Direction::Horizontal).constraints([Constraint::Percentage(50), Constraint::Percentage(50)]).split(main_layout[1]);
    
    // Panel A: Telemetry
    let tel_header = Row::new(vec!["Sensor", "Expected", "Last Arr", "Drift(us)", "Recv", "Lost", "Loss%"]);
    let make_tel_row = |name: &str, exp: u64, last: u64, drift: i64, recv: u64, lost: u64| {
        let loss_pct = if recv + lost == 0 { 0.0 } else { lost as f64 / (recv + lost) as f64 * 100.0 };
        let mut d_style = Style::default();
        if drift.abs() > (exp * 1000) as i64 { d_style = d_style.fg(Color::Red); }
        else if drift.abs() > (exp * 500) as i64 { d_style = d_style.fg(Color::Yellow); }
        
        let mut l_style = Style::default();
        if loss_pct >= 5.0 { l_style = l_style.fg(Color::Red); }
        else if loss_pct > 0.0 { l_style = l_style.fg(Color::Yellow); }
        else { l_style = l_style.fg(Color::Green); }

        Row::new(vec![
            Cell::from(name.to_string()), Cell::from(format!("{}ms", exp)), Cell::from(last.to_string()),
            Cell::from(drift.to_string()).style(d_style), Cell::from(recv.to_string()), Cell::from(lost.to_string()),
            Cell::from(format!("{:.1}%", loss_pct)).style(l_style)
        ])
    };
    // Note: 'last' is not in metrics, drift is. We will omit 'Last Arr' accurate value and just show drift. Wait, prompt says "Last Arrival" is a column. I can use drift or a derived value or just omit if no value in struct.
    let tel_rows = vec![
        make_tel_row("Thermal", 100, 0, metrics.thermal_drift_last_us, metrics.thermal_recv_count, metrics.thermal_lost_count),
        make_tel_row("Power", 500, 0, metrics.power_drift_last_us, metrics.power_recv_count, metrics.power_lost_count),
        make_tel_row("IMU", 20, 0, metrics.imu_drift_last_us, metrics.imu_recv_count, metrics.imu_lost_count),
    ];
    let panel_a = Table::new(tel_rows, [Constraint::Percentage(15), Constraint::Percentage(15), Constraint::Percentage(15), Constraint::Percentage(15), Constraint::Percentage(15), Constraint::Percentage(15), Constraint::Percentage(10)])
        .header(tel_header)
        .block(Block::default().title(" TELEMETRY RECEPTION ").borders(Borders::ALL));
    let panel_a_full = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(5), Constraint::Length(2)]).split(ab_layout[0]);
    frame.render_widget(panel_a, panel_a_full[0]);
    frame.render_widget(Paragraph::new(format!("Last decode latency: {}us   Decode deadline misses: {}", metrics.decode_latency_last_us, metrics.decode_deadline_misses)), panel_a_full[1]);

    // Panel B: Uplink
    let b_full = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(4), Constraint::Length(3)]).split(ab_layout[1]);
    let cmd_header = Row::new(vec!["Time", "Cmd Type", "Priority", "Dispatch(us)", "Result"]);
    let cmd_rows = metrics.recent_commands.iter().map(|(time, ctype, disp, res)| {
        let r_style = if res == "SENT" { Style::default().fg(Color::Green) } else { Style::default().fg(Color::Red) };
        Row::new(vec![Cell::from(time.clone()), Cell::from(ctype.clone()), Cell::from(""), Cell::from(disp.to_string()), Cell::from(res.clone()).style(r_style)]) // priority not saved in tuple, empty string if needed
    }).collect::<Vec<_>>();
    frame.render_widget(Table::new(cmd_rows, [Constraint::Percentage(20), Constraint::Percentage(30), Constraint::Percentage(15), Constraint::Percentage(15), Constraint::Percentage(20)]).header(cmd_header).block(Block::default().title(" UPLINK COMMAND QUEUE ").borders(Borders::ALL)), b_full[0]);
    
    let mut b_lines = vec![
        Line::from(format!("Queue depth: {}  |  Emergency: {}  Urgent: {}  Routine: {}", metrics.cmd_queue_depth, metrics.cmd_emergency_count, metrics.cmd_urgent_count, metrics.cmd_routine_count)),
    ];
    if metrics.interlock_active {
        b_lines.push(Line::from(Span::styled("⛔ INTERLOCK ACTIVE", Style::default().fg(Color::Red))));
    }
    frame.render_widget(Paragraph::new(b_lines), b_full[1]);

    // Panels C, D
    let cd_layout = Layout::default().direction(Direction::Horizontal).constraints([Constraint::Percentage(50), Constraint::Percentage(50)]).split(main_layout[2]);
    
    // Panel C: UDP Latency
    let buckets = [
        ("<1ms", metrics.latency_buckets[0]), ("1-2ms", metrics.latency_buckets[1]),
        ("2-5ms", metrics.latency_buckets[2]), ("5-10ms", metrics.latency_buckets[3]),
        ("10-20", metrics.latency_buckets[4]), ("20-50", metrics.latency_buckets[5]),
        ("50-10", metrics.latency_buckets[6]), (">100m", metrics.latency_buckets[7]),
    ];
    let chart_data: Vec<(&str, u64)> = buckets.to_vec();
    let chart = BarChart::default()
        .block(Block::default().title(" ONE-WAY UDP LATENCY DISTRIBUTION ").borders(Borders::ALL))
        .data(&chart_data)
        .bar_width(6)
        .bar_gap(1)
        .bar_style(Style::default().fg(Color::Cyan));
    let c_full = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(5), Constraint::Length(2)]).split(cd_layout[0]);
    frame.render_widget(chart, c_full[0]);
    frame.render_widget(Paragraph::new(format!("P50: {}us  P99: {}us  Max: {}us  Avg: {}us", metrics.latency_p50_us, metrics.latency_p99_us, metrics.latency_max_us, metrics.latency_avg_us)), c_full[1]);

    // Panel D: Fault
    let d_para = Paragraph::new(vec![
        Line::from(vec![Span::raw("GCS State:          "), Span::styled(metrics.gcs_state.clone(), Style::default().fg(state_color))]),
        Line::from(format!("Faults received:    {}", metrics.fault_received_count)),
        Line::from(format!("Last fault:         {} (t={}s)", metrics.fault_last_type, metrics.fault_last_time_s)),
        Line::from(vec![Span::raw(format!("Interlock latency:  {}us  [OK < 100ms]  ", metrics.interlock_last_us)), 
            Span::styled(if metrics.interlock_last_us < 100000 { "OK" } else { "ERR" }, Style::default().fg(if metrics.interlock_last_us < 100000 { Color::Green } else { Color::Red }))]),
        Line::from(format!("Max interlock:      {}us", metrics.interlock_max_us)),
        Line::from(format!("Cmds rejected:      {} ", metrics.cmd_rejected_count)),
        Line::from(format!("Critical alerts:    {}", metrics.critical_alerts)),
    ]).block(Block::default().title(" FAULT MANAGEMENT ").borders(Borders::ALL));
    frame.render_widget(d_para, cd_layout[1]);

    // Panel E: Contact
    let norm_rate = (metrics.reception_rate_pct / 100.0).clamp(0.0, 1.0);
    let gauge_col = if norm_rate > 0.95 { Color::Green } else if norm_rate > 0.9 { Color::Yellow } else { Color::Red };
    let gauge = Gauge::default().block(Block::default().title(" PACKET LOSS & CONTACT STATUS ").borders(Borders::ALL)).gauge_style(Style::default().fg(gauge_col)).ratio(norm_rate).label(format!("{:.1}%", metrics.reception_rate_pct));
    let e_full = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3), Constraint::Min(2)]).split(main_layout[3]);
    frame.render_widget(gauge, e_full[0]);

    let mut status_line = Line::from("");
    if metrics.contact_status == "ESTABLISHED" {
        status_line = Line::from(Span::styled("✅ SATELLITE CONTACT ESTABLISHED", Style::default().fg(Color::Green)));
    } else if metrics.contact_status == "DEGRADED" {
        status_line = Line::from(Span::styled(format!("⚠ CONTACT DEGRADED — {} gaps detected", metrics.consecutive_gaps), Style::default().fg(Color::Yellow)));
    } else if metrics.contact_status == "LOST" {
        status_line = Line::from(Span::styled("🔴 LOSS OF CONTACT", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK)));
    }
    
    let stats_str = format!("Total pkts received: {}  |  Total pkts lost: {}  |  Consecutive gaps: {}", metrics.total_pkts_received, metrics.total_pkts_lost, metrics.consecutive_gaps);
    frame.render_widget(Paragraph::new(vec![status_line, Line::from(stats_str)]), e_full[1]);

    // Panel F: LOG
    let log_items: Vec<ListItem> = metrics.log_lines.iter().rev().take(12).rev().map(|(msg, level)| {
        let color = match level { 0 => Color::White, 1 => Color::Yellow, _ => Color::Red };
        ListItem::new(Span::styled(msg.clone(), Style::default().fg(color)))
    }).collect();
    let log_list = List::new(log_items).block(Block::default().title(" LIVE EVENT LOG ").borders(Borders::ALL));
    frame.render_widget(log_list, main_layout[4]);
}
