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
pub struct SatMetricsSnapshot {
    // Sensor jitter
    pub thermal_jitter_last_us: u64,
    pub thermal_jitter_p50_us:  u64,
    pub thermal_jitter_p99_us:  u64,
    pub thermal_jitter_max_us:  u64,
    
    pub power_jitter_last_us:   u64,
    pub power_jitter_p50_us:    u64,
    pub power_jitter_p99_us:    u64,
    pub power_jitter_max_us:    u64,
    
    pub imu_jitter_last_us:     u64,
    pub imu_jitter_p50_us:      u64,
    pub imu_jitter_p99_us:      u64,
    pub imu_jitter_max_us:      u64,

    // Buffer
    pub buffer_len:             usize,
    pub buffer_capacity:        usize,
    pub buffer_fill_pct:        f64,
    pub buffer_total_dropped:   u64,
    pub buffer_degraded:        bool,
    pub thermal_consecutive_miss: u32,

    // System state
    pub system_state:           String,   // "NOMINAL", "DEGRADED", "FAULT", "MISSION_ABORT"

    // Scheduler
    pub thermal_ctrl_drift_us:  i64,
    pub data_compress_drift_us: i64,
    pub health_monitor_drift_us: i64,
    pub thermal_ctrl_violations: u64,
    pub data_compress_violations: u64,
    pub health_monitor_violations: u64,
    pub cpu_util_pct:           f64,

    // Downlink
    pub downlink_queue_latency_sparkline: std::collections::VecDeque<u64>,  // cap 60
    pub downlink_queue_p50_us:  u64,
    pub downlink_queue_p99_us:  u64,
    pub downlink_queue_max_us:  u64,
    pub downlink_total_sent:    u64,
    pub downlink_window_violations: u64,

    // Fault engine
    pub fault_total_injected:   u64,
    pub fault_next_in_s:        u64,
    pub fault_last_type:        String,
    pub fault_last_recovery_ms: u64,
    pub fault_max_recovery_ms:  u64,
    pub fault_circuit_state:    String,  // "CLOSED", "OPEN", "HALF-OPEN"
    pub mission_aborts:         u64,

    // Live log
    pub log_lines: std::collections::VecDeque<(String, u8)>, // (text, severity: 0=info,1=warn,2=error)
}

impl Default for SatMetricsSnapshot {
    fn default() -> Self {
        Self {
            thermal_jitter_last_us: 0, thermal_jitter_p50_us: 0, thermal_jitter_p99_us: 0, thermal_jitter_max_us: 0,
            power_jitter_last_us: 0, power_jitter_p50_us: 0, power_jitter_p99_us: 0, power_jitter_max_us: 0,
            imu_jitter_last_us: 0, imu_jitter_p50_us: 0, imu_jitter_p99_us: 0, imu_jitter_max_us: 0,
            buffer_len: 0, buffer_capacity: 64, buffer_fill_pct: 0.0, buffer_total_dropped: 0, buffer_degraded: false, thermal_consecutive_miss: 0,
            system_state: "NOMINAL".to_string(),
            thermal_ctrl_drift_us: 0, data_compress_drift_us: 0, health_monitor_drift_us: 0,
            thermal_ctrl_violations: 0, data_compress_violations: 0, health_monitor_violations: 0, cpu_util_pct: 0.0,
            downlink_queue_latency_sparkline: std::collections::VecDeque::with_capacity(60),
            downlink_queue_p50_us: 0, downlink_queue_p99_us: 0, downlink_queue_max_us: 0, downlink_total_sent: 0, downlink_window_violations: 0,
            fault_total_injected: 0, fault_next_in_s: 0, fault_last_type: "None".to_string(), fault_last_recovery_ms: 0, fault_max_recovery_ms: 0, fault_circuit_state: "CLOSED".to_string(), mission_aborts: 0,
            log_lines: std::collections::VecDeque::with_capacity(200),
        }
    }
}

pub fn push_log(
    metrics: &Arc<Mutex<SatMetricsSnapshot>>,
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
    metrics: Arc<Mutex<SatMetricsSnapshot>>,
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

                // Check for 'q' to quit (non-blocking)
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

fn render_dashboard(frame: &mut Frame, metrics: &SatMetricsSnapshot, elapsed: Duration) {
    let main_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Title
            Constraint::Length(7), // A, B, C
            Constraint::Length(6), // D: RMS SCHEDULER
            Constraint::Length(5), // E: DOWNLINK
            Constraint::Length(7), // F: FAULT
            Constraint::Min(12),   // G: LOG
        ])
        .split(frame.size());

    // Header
    let state_color = match metrics.system_state.as_str() {
        "NOMINAL" => Color::Green,
        "DEGRADED" => Color::Yellow,
        "FAULT" => Color::Red,
        "MISSION_ABORT" | _ => Color::Red,
    };
    let state_style = if metrics.system_state == "MISSION_ABORT" {
        Style::default().fg(state_color).add_modifier(Modifier::BOLD | Modifier::SLOW_BLINK)
    } else {
        Style::default().fg(state_color).add_modifier(Modifier::BOLD)
    };
    let title = Paragraph::new(Line::from(vec![
        Span::raw("  SATELLITE OCS DASHBOARD  [sim elapsed: "),
        Span::raw(format!("{:02}:{:02}:{:02}]  [state: ", elapsed.as_secs()/3600, (elapsed.as_secs()%3600)/60, elapsed.as_secs()%60)),
        Span::styled(metrics.system_state.clone(), state_style),
        Span::raw("]"),
    ]))
    .block(Block::default().borders(Borders::ALL));
    frame.render_widget(title, main_layout[0]);

    // Panels A, B, C Row
    let top_panels = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(45), // A
            Constraint::Percentage(30), // B
            Constraint::Percentage(25), // C
        ])
        .split(main_layout[1]);

    // Panel A: Sensor Jitter
    let sensor_header = Row::new(vec!["Sensor", "Period", "Last(us)", "p50(us)", "p99(us)", "Max(us)", "Limit(us)", "Status"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    
    let make_jitter_row = |name: &str, period: &str, last: u64, p50: u64, p99: u64, max: u64, limit: u64| {
        let mut row_style = Style::default();
        if name == "Thermal" && last > limit {
            row_style = row_style.fg(Color::Red);
        }
        let status = if last > limit {
            Span::styled("🔴 CRIT", Style::default().fg(Color::Red))
        } else if p99 > limit / 2 {
            Span::styled("⚠ WARN", Style::default().fg(Color::Yellow))
        } else {
            Span::styled("OK", Style::default().fg(Color::Green))
        };
        Row::new(vec![
            Cell::from(name.to_string()), Cell::from(period.to_string()),
            Cell::from(last.to_string()), Cell::from(p50.to_string()), Cell::from(p99.to_string()),
            Cell::from(max.to_string()), Cell::from(limit.to_string()),
            Cell::from(status)
        ]).style(row_style)
    };

    let sensor_rows = vec![
        make_jitter_row("Thermal", "100ms", metrics.thermal_jitter_last_us, metrics.thermal_jitter_p50_us, metrics.thermal_jitter_p99_us, metrics.thermal_jitter_max_us, 1000),
        make_jitter_row("Power", "500ms", metrics.power_jitter_last_us, metrics.power_jitter_p50_us, metrics.power_jitter_p99_us, metrics.power_jitter_max_us, 5000),
        make_jitter_row("IMU", "20ms", metrics.imu_jitter_last_us, metrics.imu_jitter_p50_us, metrics.imu_jitter_p99_us, metrics.imu_jitter_max_us, 500),
    ];
    let panel_a = Table::new(sensor_rows, [Constraint::Min(8), Constraint::Min(8), Constraint::Min(8), Constraint::Min(8), Constraint::Min(8), Constraint::Min(8), Constraint::Min(9), Constraint::Min(7)])
        .header(sensor_header)
        .block(Block::default().title(" SENSOR JITTER ").borders(Borders::ALL));
    frame.render_widget(panel_a, top_panels[0]);

    // Panel B: Buffer Status
    let fill_norm = (metrics.buffer_fill_pct / 100.0).clamp(0.0, 1.0);
    let gauge_color = if fill_norm < 0.6 { Color::Green } else if fill_norm < 0.8 { Color::Yellow } else { Color::Red };
    let gauge = Gauge::default()
        .block(Block::default().title(" BUFFER STATUS ").borders(Borders::ALL))
        .gauge_style(Style::default().fg(gauge_color))
        .ratio(fill_norm)
        .label(format!("{:.1}%", metrics.buffer_fill_pct));
    
    let buffer_layout = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(3), Constraint::Min(2)]).split(top_panels[1]);
    frame.render_widget(gauge, buffer_layout[0]);
    
    let buf_text = Paragraph::new(vec![
        Line::from(format!("Capacity: {} / {}   Dropped total: {}", metrics.buffer_len, metrics.buffer_capacity, metrics.buffer_total_dropped)),
        Line::from(format!("Degraded mode: {}", if metrics.buffer_degraded { "YES" } else { "NO" })),
    ]);
    frame.render_widget(buf_text, buffer_layout[1]);

    // Panel C: System State
    let sys_state_p = Paragraph::new(vec![
        Line::from(Span::styled(metrics.system_state.clone(), state_style)),
        Line::from(format!("Consecutive thermal misses: {}", metrics.thermal_consecutive_miss)),
    ]).block(Block::default().title(" SYSTEM STATE ").borders(Borders::ALL));
    frame.render_widget(sys_state_p, top_panels[2]);

    // Panel D: RMS Scheduler
    let sched_header = Row::new(vec!["Task", "Priority", "Period(ms)", "WCET(ms)", "Last Drift(us)", "Violations", "CPU %"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    let make_sched_row = |name: &str, prio: &str, period: u64, wcet: u64, drift: i64, viol: u64, cpu: f64| {
        let mut style = Style::default();
        if drift > (wcet * 1000) as i64 {
            style = style.fg(Color::Red);
        }
        Row::new(vec![
            name.to_string(), prio.to_string(), period.to_string(), wcet.to_string(),
            drift.to_string(), viol.to_string(), format!("{:.1}%", cpu)
        ]).style(style)
    };
    let sched_rows = vec![
        make_sched_row("Thermal Ctrl", "High", 100, 2, metrics.thermal_ctrl_drift_us, metrics.thermal_ctrl_violations, metrics.cpu_util_pct), // CPU is overall for now or individual if we track it. The prompt says "CPU % is calculated as active_ticks / total_ticks". We show it across tasks or generic. We will show generic.
        make_sched_row("Data Compress", "Medium", 200, 10, metrics.data_compress_drift_us, metrics.data_compress_violations, 0.0),
        make_sched_row("Health Monitor", "Low", 1000, 5, metrics.health_monitor_drift_us, metrics.health_monitor_violations, 0.0),
    ];
    let panel_d = Table::new(sched_rows, [Constraint::Percentage(20), Constraint::Percentage(15), Constraint::Percentage(15), Constraint::Percentage(15), Constraint::Percentage(13), Constraint::Percentage(12), Constraint::Percentage(10)])
        .header(sched_header)
        .block(Block::default().title(" RMS SCHEDULER ").borders(Borders::ALL));
    frame.render_widget(panel_d, main_layout[2]);

    // Panel E: Downlink TX
    let dl_layout = Layout::default().direction(Direction::Horizontal).constraints([Constraint::Percentage(70), Constraint::Percentage(30)]).split(main_layout[3]);
    let spark_data: Vec<u64> = metrics.downlink_queue_latency_sparkline.iter().copied().collect();
    let sparkline = Sparkline::default()
        .block(Block::default().title(" DOWNLINK TX (Latency µs) ").borders(Borders::ALL))
        .data(&spark_data)
        .style(Style::default().fg(Color::Blue));
    frame.render_widget(sparkline, dl_layout[0]);
    
    let mut dl_text = vec![
        Line::from(format!("P50: {}us  P99: {}us  Max: {}us", metrics.downlink_queue_p50_us, metrics.downlink_queue_p99_us, metrics.downlink_queue_max_us)),
        Line::from(format!("Sent: {} pkts  Window violations: {}", metrics.downlink_total_sent, metrics.downlink_window_violations)),
    ];
    if metrics.buffer_fill_pct > 80.0 {
        dl_text.push(Line::from(Span::styled("DEGRADED MODE ACTIVE", Style::default().fg(Color::Yellow))));
    }
    let dl_para = Paragraph::new(dl_text).block(Block::default().borders(Borders::ALL));
    frame.render_widget(dl_para, dl_layout[1]);

    // Panel F: Fault Engine
    let rec_str = format!("{}ms", metrics.fault_last_recovery_ms);
    let rec_color = if metrics.fault_last_recovery_ms < 200 { Color::Green } else { Color::Red };
    let cb_color = match metrics.fault_circuit_state.as_str() {
        "CLOSED" => Color::Green,
        "HALF-OPEN" => Color::Yellow,
        "OPEN" | _ => Color::Red,
    };
    let fault_para = Paragraph::new(vec![
        Line::from(format!("Faults injected:  {}      Next fault in: {}s", metrics.fault_total_injected, metrics.fault_next_in_s)),
        Line::from(format!("Last fault type:  {}", metrics.fault_last_type)),
        Line::from(vec![Span::raw("Last recovery:    "), Span::styled(rec_str, Style::default().fg(rec_color)), Span::raw("  [OK < 200ms]")] ),
        Line::from(format!("Max recovery:     {}ms", metrics.fault_max_recovery_ms)),
        Line::from(vec![Span::raw("Circuit breaker:  "), Span::styled(metrics.fault_circuit_state.clone(), Style::default().fg(cb_color))] ),
        Line::from(format!("Mission aborts:   {}", metrics.mission_aborts)),
    ]).block(Block::default().title(" FAULT ENGINE ").borders(Borders::ALL));
    frame.render_widget(fault_para, main_layout[4]);

    // Panel G: Live Event Log
    let log_items: Vec<ListItem> = metrics.log_lines.iter().rev().take(12).rev().map(|(msg, level)| {
        let color = match level {
            0 => Color::White,
            1 => Color::Yellow,
            _ => Color::Red,
        };
        ListItem::new(Span::styled(msg.clone(), Style::default().fg(color)))
    }).collect();
    let log_list = List::new(log_items).block(Block::default().title(" LIVE EVENT LOG ").borders(Borders::ALL));
    frame.render_widget(log_list, main_layout[5]);
}
