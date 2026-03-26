GCS 5-Minute Full Exercise-Mode System Test Proof

Run date: 2026-03-26
Workspace: C:\Users\User\Documents\APU\RTS\Assignment\Satellite_Sim\satellite-gcs-rts
Mode:
- `SAT_SIM_EXERCISE_GCS_EDGE_CASES=1`
- one clean 300 s GCS run
- one clean 300 s satellite run

Artifacts
- `ground.log`
- `satellite.log`
- `gcs_final_report.txt`

Run summary from `gcs_final_report.txt`
- `time_end_us=300069620`
- `total_pkts_received=4885`
- `total_pkts_lost=161`
- `reception_rate_pct=96.809`
- `decode_deadline_misses=0`
- `latency_p50_us=187`
- `latency_p99_us=267`
- `latency_max_us=1102`
- `re_request_count=12`
- `delayed_packet_events=3`
- `telemetry_backlog_max=9`
- `cmd_total_sent=68`
- `cmd_deadline_misses=0`
- `cmd_rejected_count=5`
- `cmd_rejection_last_reason=interlock_active_non_emergency_blocked`
- `fault_received_count=3`
- `interlock_max_us=63`
- `critical_alerts=0`

Core proof

1. Telemetry Reception and Decoding
- Socket telemetry receive + decode under 3 ms:
  - `ground.log:6` `telemetry_rx sensor=Thermal latency_us=228 drift_us=0 decode_us=1`
  - `ground.log:8` `telemetry_rx sensor=Power latency_us=92 drift_us=0 decode_us=0`
  - `ground.log:13` `telemetry_rx sensor=Imu latency_us=123 drift_us=0 decode_us=0`
  - report: `decode_deadline_misses=0`
- Corrected latency remains trustworthy:
  - report: `latency_p50_us=187`, `latency_p99_us=267`, `latency_max_us=1102`
  - late-run samples remain in microseconds, not inflated:
    - `ground.log:4839` `latency_us=165`
    - `ground.log:4859` `latency_us=1455`
- Missing/delayed packet re-request path:
  - `ground.log:1562` `RequestTelemetry enqueued sensor=Thermal missing_from=899`
  - `ground.log:1565` `RequestTelemetry enqueued sensor=Power missing_from=449`
  - `ground.log:1567` `RequestTelemetry enqueued sensor=Imu missing_from=179`
  - `ground.log:1642` `PACKET LOSS DETECTED ... sensor=Thermal expected=963 got=1020 gap=57`
  - `ground.log:1647` `PACKET LOSS DETECTED ... sensor=Power expected=449 got=510 gap=61`
  - `ground.log:1655` `PACKET LOSS DETECTED ... sensor=Imu expected=179 got=204 gap=25`
  - satellite executed re-requests:
    - `satellite.log:4291` `uplink_rx: received cmd=RequestTelemetry seq=899`
    - `satellite.log:4901` `uplink_rx: received cmd=RequestTelemetry seq=963`
    - `satellite.log:4909` `uplink_rx: received cmd=RequestTelemetry seq=449`
    - `satellite.log:4926` `uplink_rx: received cmd=RequestTelemetry seq=179`

2. Explicit GCS Loss Of Contact
- Delayed-packet loss-of-contact trigger:
  - `ground.log:1568` `SATELLITE LOSS OF CONTACT: 3+ delayed packets in sequence fails=3`
- Explicit receive-timeout loss-of-contact trigger:
  - `ground.log:1571` `SATELLITE LOSS OF CONTACT: receive timeout exceeded since_last_us=10033894 limit_us=10000000`
- Satellite blackout used to exercise the path:
  - `satellite.log:4281` `EXERCISE: telemetry blackout start to trigger GCS loss of contact`
  - `satellite.log:4828` `EXERCISE: telemetry blackout end`

3. Command Rejected
- Rejections under loss-of-contact:
  - `ground.log:1569` `COMMAND REJECTED cmd=RequestTelemetry reason="loss_of_contact_non_emergency_blocked"`
  - `ground.log:1570` `COMMAND REJECTED cmd=RequestTelemetry reason="loss_of_contact_non_emergency_blocked"`
  - `ground.log:1572` `telemetry_rx: exercise command enqueued to prove loss-of-contact rejection`
  - `ground.log:1574` `COMMAND REJECTED cmd=AdjustAntenna reason="loss_of_contact_non_emergency_blocked"`
- Rejections under interlock:
  - `ground.log:1978` `COMMAND REJECTED cmd=RequestTelemetry reason="interlock_active_non_emergency_blocked"`
  - `ground.log:4073` `COMMAND REJECTED cmd=RequestTelemetry reason="interlock_active_non_emergency_blocked"`
- report:
  - `cmd_rejected_count=5`
  - `cmd_rejection_last_reason=interlock_active_non_emergency_blocked`

4. Critical Ground Alert
- Fault received and artificial interlock delay applied:
  - `ground.log:1041` `fault_mgr: fault received fault=DelayedSensor`
  - `ground.log:1042` `fault_mgr: injecting interlock delay for exercise injected_delay_ms=105`
- Interlock applied above 100 ms threshold:
  - `ground.log:1045` `fault_mgr: interlock APPLIED interlock_latency_us=118691`
- Critical alert fired:
  - `ground.log:1046` `CRITICAL GROUND ALERT: interlock exceeded 100ms interlock_latency_us=118691 limit_us=100000`

5. Fault Management And Recovery
- Fault injection and recovery on satellite:
  - `satellite.log:2857` `FAULT INJECTED fault=DelayedSensor`
  - `satellite.log:2874` `uplink_rx: received cmd=ResetSensor`
  - `satellite.log:2876` `FAULT RECOVERED ... recovery_ms=181`
  - `satellite.log:5774` `FAULT INJECTED fault=CorruptedData`
  - `satellite.log:5796` `uplink_rx: received cmd=ResetSensor`
  - `satellite.log:5801` `FAULT RECOVERED ... recovery_ms=81`
  - `satellite.log:8661` `FAULT INJECTED fault=DelayedSensor`
  - `satellite.log:8675` `FAULT RECOVERED ... recovery_ms=91`
  - `satellite.log:11529` `FAULT INJECTED fault=CorruptedData`
  - `satellite.log:11549` `FAULT RECOVERED ... recovery_ms=61`
- satellite final rollup:
  - `satellite.log:14360` `=== SATELLITE SIMULATION COMPLETE === ... total_faults_injected=3 max_recovery_ms=181 mission_aborts=0`

6. Performance Monitoring
- periodic performance reports:
  - `ground.log:467` `=== PERFORMANCE REPORT === ...`
  - `ground.log:1905` `=== PERFORMANCE REPORT === ...`
  - `ground.log:5049` `=== PERFORMANCE REPORT === ...`
- final GCS rollup:
  - `ground.log:5051` `=== GCS SIMULATION COMPLETE === total_pkts_received=4885 total_pkts_lost=161 ...`
- task drift / jitter / backlog / pipeline:
  - report: `uplink_jitter_p99_us=12009471`
  - report: `telemetry_backlog_max=9`
  - report: `task_drift_uplink_last_us=10918`
  - report: `task_drift_telemetry_last_us=-110026`
  - report: `task_drift_fault_last_us=-2473`
  - report: `pipeline_packet_to_uplink_last_us=165`
  - report: `pipeline_command_to_response_last_us=11885858`

Important inconsistency
- The proof for `CRITICAL GROUND ALERT` is present in `ground.log` at line `1046`.
- However, `gcs_final_report.txt` still shows `critical_alerts=0` and `interlock_max_us=63`.
- That is inconsistent with the authoritative runtime log line `interlock_latency_us=118691` and indicates a report aggregation bug, not a missing event.

Assessment
- Proven in this 5-minute exercise run:
  - telemetry receive and decode timing
  - trustworthy latency logging
  - packet re-request path
  - explicit GCS `LOSS OF CONTACT`
  - explicit `COMMAND REJECTED`
  - explicit `CRITICAL GROUND ALERT`
  - satellite fault injection and recovery
  - periodic performance reporting
- Remaining issue:
  - `gcs_final_report.txt` under-reports the exercised critical-alert event
