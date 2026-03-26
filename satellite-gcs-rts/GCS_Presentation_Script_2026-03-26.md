# GCS Presentation Script - 2026-03-26

## Opening

"Today I am presenting the Ground Control Station, or GCS, side of the satellite simulation. This implementation uses real TCP sockets, not mock function calls. In `ground/src/main.rs:79-80` the GCS binds a TCP listener, and in `ground/src/main.rs:153-213` it starts the three core runtime tasks: `telemetry_rx`, `uplink_tx`, and `fault_mgr`."

"The logging is also deliberate. In `ground/src/main.rs:44-58` the tracing subscriber is configured to write timestamped events into `ground.log`. That is why every real-time action in the log starts with an ISO timestamp and can be used as runtime proof."

## 1. Telemetry Reception and Decoding

"For the first requirement, telemetry reception and decoding, the socket receive loop is in `ground/src/telemetry_rx.rs:72-75`. The decode happens in `ground/src/telemetry_rx.rs:206-218`. If decoding ever exceeds 3 milliseconds, the code logs `DECODE DEADLINE MISSED` and increments the `decode_deadline_misses` counter."

"Latency and reception drift are measured immediately after decode. The latency calculation is in `ground/src/telemetry_rx.rs:236-239`, and the drift calculation and telemetry logging are in `ground/src/telemetry_rx.rs:242-258`. The per-sensor counters and the pipeline latency metric are then stored in `ground/src/telemetry_rx.rs:264-271`."

"For missing or delayed packets, there are two detection paths. Delayed packets are detected in `ground/src/telemetry_rx.rs:112-141`. Actual sequence gaps are detected in `ground/src/telemetry_rx.rs:273-291`. In both cases the GCS enqueues `RequestTelemetry`, and that command is built with urgent priority in `ground/src/telemetry_rx.rs:355-379`."

"Loss of contact is also implemented in code. A receive-timeout loss-of-contact path is in `ground/src/telemetry_rx.rs:78-90`. The three-fail-in-sequence path is in `ground/src/telemetry_rx.rs:143-156` and `ground/src/telemetry_rx.rs:323-332`."

"For proof from the clean final run, I show `ground.log:6-13`. Those lines contain `telemetry_rx` entries with `latency_us`, `drift_us`, and `decode_us`. Then I show `gcs_final_report.txt:10-16`, where `decode_deadline_misses=0`, `re_request_count=5`, and `telemetry_backlog_max=5`."

"For proof that the re-request path really executed in the clean run, I show `ground.log:2091-2100` and `ground.log:4188-4205`. Those lines show `PACKET LOSS DETECTED`, `RequestTelemetry enqueued`, and the subsequent uplink send. I can cross-check that on the satellite side with `satellite.log:5747-5758` and `satellite.log:11501-11530`, where `RequestTelemetry` is received and executed."

"For proof of the forced loss-of-contact branch, I use the dedicated exercise artifact `gcs_system_test_proof_2026-03-26.md:61-68`. That document records both the delayed-packet trigger and the receive-timeout trigger."

## 2. Command Uplink Scheduler

"For the command uplink scheduler, the real-time command queue is a priority queue. It is created in `ground/src/main.rs:62-63`, and the priority ordering is defined in `ground/src/uplink_tx.rs:17-21` using `BinaryHeap`."

"The actual uplink scheduler runs in `ground/src/uplink_tx.rs:58-76`. It wakes every 5 milliseconds, measures task drift, and pops the next highest-priority command."

"Urgent deadline enforcement is in `ground/src/uplink_tx.rs:102-119`. The deadline is set to 2 milliseconds for urgent or emergency commands. If dispatch exceeds that limit, `ground/src/uplink_tx.rs:132-137` logs `DISPATCH DEADLINE MISSED` and increments the counter."

"Safety validation against system state is in `ground/src/uplink_tx.rs:78-99`. When the GCS state is `InterlockActive` or `LossOfContact`, non-emergency commands are rejected. The GCS states themselves are defined in `ground/src/state.rs:1-7`."

"For proof from the clean final run, I show `ground.log:5`, `ground.log:1043-1045`, `ground.log:2094-2100`, and `ground.log:4190-4205`. Those lines show real commands being scheduled and sent, with `dispatch_us` values in the hundreds of microseconds, which is below the 2 millisecond urgent deadline. Then I show `gcs_final_report.txt:19-25`, where `cmd_deadline_misses=0`."

"One honest point here is that the clean final run has `cmd_rejected_count=0` in `gcs_final_report.txt:21-22`. That is correct for that run because no unsafe operator command was sent while the GCS was in interlock or loss-of-contact. To prove the rejection path itself, I use the exercise artifact `gcs_system_test_proof_2026-03-26.md:70-81`, which records `COMMAND REJECTED` under both loss-of-contact and interlock conditions."

## 3. Fault Management and Interlocks

"For fault management, telemetry routing sends fault packets to the fault manager in `ground/src/telemetry_rx.rs:193-198`. The fault manager receives them in `ground/src/fault_mgr.rs:44-50`."

"The interlock is applied in `ground/src/fault_mgr.rs:64-75`. That code records the detection time, changes the system state to `InterlockActive`, computes `interlock_latency_us`, and logs `fault_mgr: interlock APPLIED`."

"The critical-ground-alert threshold is implemented in `ground/src/fault_mgr.rs:77-84`. The important unit is microseconds. The requirement says the alert should fire above 100 milliseconds, so the threshold in code is `100000` microseconds."

"Unsafe-command blocking is handled by the uplink scheduler, not by a separate validator. That is the `ground/src/uplink_tx.rs:78-99` branch I mentioned earlier. During interlock, non-emergency commands are refused and the rejection reason is logged."

"The actual safety-response commands are generated in `ground/src/fault_mgr.rs:86-93` for `SafeMode`, and in `ground/src/fault_mgr.rs:100-121` for `ResetSensor` after the interlock window clears."

"For proof from the clean final run, I show `ground.log:1041-1045`, `ground.log:2085-2089`, `ground.log:3136-3140`, and `ground.log:4182-4186`. Those line groups prove that faults were received, interlocks were applied, and `SafeMode` and `ResetSensor` were sent. I then show `gcs_final_report.txt:35-37`, where `fault_received_count=4`, `interlock_max_us=205`, and `critical_alerts=0`."

"It is important to explain that `interlock_max_us=205` means 205 microseconds, not 205 milliseconds. That is why the clean final run correctly does not trigger a critical ground alert."

"If the evaluator asks for proof that the `CRITICAL GROUND ALERT` branch can fire, I use `gcs_system_test_proof_2026-03-26.md:83-90`. That exercised run deliberately delayed interlock application beyond 100 milliseconds and captured the alert in the runtime log."

## 4. System Performance Monitoring

"For performance monitoring, the monitoring task starts in `ground/src/main.rs:90-96`. The per-second system-load calculation is in `ground/src/perf_monitor.rs:22-33`. This load is based on telemetry and uplink busy time, not on total OS CPU usage, and that is explained in `ground/src/main.rs:84-85`."

"The periodic performance report is emitted every 30 seconds in `ground/src/perf_monitor.rs:36-57`. That report includes `decode_deadline_misses`, `fault_received_count`, `cmd_deadline_misses`, `uplink_jitter_p99_us`, `telemetry_backlog_max`, `task_drift_uplink_last_us`, `task_drift_telemetry_last_us`, and `system_load_pct`."

"The final report file is generated in `ground/src/main.rs:304-377`. That is the code that writes `gcs_final_report.txt` as a structured handoff artifact for the evaluator."

"For proof from the clean final run, I show the recurring `=== PERFORMANCE REPORT ===` lines in `ground.log:484`, `ground.log:1001`, `ground.log:1523`, `ground.log:2040`, `ground.log:2570`, `ground.log:3087`, `ground.log:3609`, `ground.log:4126`, `ground.log:4660`, and `ground.log:5178`. Then I show `gcs_final_report.txt:28-40`, where the final metrics are summarized."

"A useful explanation point is that `uplink_jitter_p99_us` is being measured and reported exactly as required, but the number is dominated by sparse command traffic such as 5-second heartbeats. So the metric is valid as instrumentation, even though the value is driven by workload spacing rather than a strict 5 millisecond always-busy uplink stream."

"For the logging requirement, I point back to `ground/src/main.rs:44-58` and then simply show any runtime log line, for example `ground.log:5` or `ground.log:1041`. Every real-time action is timestamped."

## Closing

"So my GCS meets the assignment in four layers. First, it receives and decodes telemetry over real sockets and measures latency and drift. Second, it maintains a priority-based real-time uplink scheduler with deadline checks and rejection reasons. Third, it handles satellite fault notifications with interlocks, safe-mode commands, and reset commands. Fourth, it continuously reports system performance and writes a final report."

"The clean final run is the proof for the normal operating path and the automatic fault-and-recovery path. The separate exercise-mode proof document is the proof for forced edge cases like loss of contact, command rejection, and critical ground alert."

## Fast demo note

- If you want the strongest live story, use the clean final run for normal operation and the current metrics in `gcs_final_report.txt`.
- If the evaluator asks specifically about `LOSS OF CONTACT`, `COMMAND REJECTED`, or `CRITICAL GROUND ALERT`, switch to `gcs_system_test_proof_2026-03-26.md` and say that those branches were exercised in a dedicated test run so the clean final demo could stay violation-free.
