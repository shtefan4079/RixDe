#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
RIX_BUILD_PROFILE="${RIX_BUILD_PROFILE:-release}"
case "$RIX_BUILD_PROFILE" in
    release|debug) ;;
    *) echo "RIX_BUILD_PROFILE must be release or debug" >&2; exit 1 ;;
esac
export RIX_BUILD_PROFILE
if [[ -f "$ROOT_DIR/Cargo.toml" ]]; then
    BIN_DIR="$ROOT_DIR/target/$RIX_BUILD_PROFILE"
    DATA_DIR="$ROOT_DIR/assets"
else
    BIN_DIR="$SCRIPT_DIR"
    DATA_DIR="$(cd "$SCRIPT_DIR/../../share" && pwd)"
fi
rix_path_entries=("$BIN_DIR" "$SCRIPT_DIR")
if [[ "${HOME:-}" == /* && "$HOME" != *:* && "$HOME" != *$'\n'* && "$HOME" != *$'\r'* ]]; then
    for rix_user_bin in .local/bin .opencode/bin .cargo/bin .bun/bin; do
        rix_path_entries+=("$HOME/$rix_user_bin")
    done
fi
IFS=: read -r -a rix_inherited_path <<< "${PATH:-/usr/local/bin:/usr/bin:/bin}"
rix_path_entries+=("${rix_inherited_path[@]}")
rix_session_path=""
for rix_path_entry in "${rix_path_entries[@]}"; do
    [[ "$rix_path_entry" == /* && "$rix_path_entry" != *:* ]] || continue
    case ":$rix_session_path:" in
        *":$rix_path_entry:"*) continue ;;
    esac
    rix_session_path="${rix_session_path:+$rix_session_path:}$rix_path_entry"
done
export PATH="$rix_session_path"
unset rix_path_entries rix_inherited_path rix_session_path rix_path_entry rix_user_bin
export XDG_DATA_DIRS="$DATA_DIR:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"

if [[ ! -x "$BIN_DIR/rix-comp" || ! -x "$BIN_DIR/rix-shell" ]]; then
    echo "Build RixDE first: ./scripts/rix-build-release --jobs 2" >&2
    exit 1
fi

if [[ "${RIX_THEME_RESOURCES_READY:-}" != 1 && -x "$SCRIPT_DIR/rix-theme-resources" ]]; then
    exec "$SCRIPT_DIR/rix-theme-resources" --data-dir "$DATA_DIR" --exec "$0" "$@"
fi

rix_session_reentry="${RIX_SESSION_REENTRY:-0}"
unset RIX_SESSION_REENTRY
if [[ "$rix_session_reentry" != 1 ]]; then unset RIX_SESSION_BUS_ISOLATED; fi

mode=auto
isolated=0
log_session=0
args=()
for arg in "$@"; do
    case "$arg" in
        --native|--drm)
            [[ "$mode" != nested ]] || { echo 'Choose either --native or --nested' >&2; exit 2; }
            mode=native ;;
        --nested)
            [[ "$mode" != native ]] || { echo 'Choose either --native or --nested' >&2; exit 2; }
            mode=nested ;;
        --isolated) isolated=1 ;;
        --log-session) log_session=1 ;;
        --help|-h)
            echo 'Usage: rix-session [--nested] [--isolated] [compositor options]'
            echo '       rix-session --native --experimental-kms [compositor options]'
            echo '       --log-session saves output in $XDG_STATE_HOME/rix/session.log'
            exit 0 ;;
        *) args+=("$arg") ;;
    esac
done
if [[ "$log_session" == 1 ]]; then
    state_home="${XDG_STATE_HOME:-$HOME/.local/state}"
    [[ "$state_home" == /* ]] || state_home="$HOME/.local/state"
    state_dir="$state_home/rix"
    saved_umask=$(umask)
    umask 077
    mkdir -p -- "$state_dir"
    chmod 700 -- "$state_dir"
    if [[ -f "$state_dir/session.log" ]]; then
        mv -f -- "$state_dir/session.log" "$state_dir/session.previous.log"
    fi
    exec >"$state_dir/session.log" 2>&1
    umask "$saved_umask"
    printf 'RixDE session started: %s\n' "$(date -Is)"
fi
if [[ "$mode" == auto ]]; then
    if [[ -n "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ]]; then mode=nested; else mode=native; fi
fi
if [[ "$isolated" == 1 && "$mode" == nested ]]; then
    if [[ "$rix_session_reentry" != 1 ]]; then
        case "${RIX_IPC_SOCKET:-}" in
            "${XDG_RUNTIME_DIR:-/tmp}"/rix-ipc-*.sock) unset RIX_IPC_SOCKET ;;
        esac
    fi
    export RIX_IPC_SOCKET="${RIX_IPC_SOCKET:-${XDG_RUNTIME_DIR:-/tmp}/rix-ipc-$$.sock}"
fi
dbus_service_available() {
    local service="$1"
    local data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
    local data_dirs="${XDG_DATA_DIRS:-/usr/local/share:/usr/share}"
    local dir
    local -a search_dirs=("$data_home")
    local -a system_dirs=()
    IFS=: read -r -a system_dirs <<< "$data_dirs"
    search_dirs+=("${system_dirs[@]}")
    for dir in "${search_dirs[@]}"; do
        [[ -r "$dir/dbus-1/services/$service" ]] && return 0
    done
    return 1
}

secret_store="${RIX_SECRET_STORE:-}"
if [[ -z "$secret_store" ]]; then
    secret_preference="${XDG_CONFIG_HOME:-$HOME/.config}/rix/secret-store"
    if [[ -e "$secret_preference" || -L "$secret_preference" ]]; then
        secret_store=$(/usr/bin/python3 -I - "$secret_preference" <<'PY'
import os, stat, sys
try:
    directory = os.open(os.path.dirname(sys.argv[1]), os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW | os.O_CLOEXEC)
    try:
        meta = os.fstat(directory)
        if meta.st_uid != os.geteuid() or meta.st_mode & 0o022:
            raise ValueError("unprotected provider directory")
        descriptor = os.open("secret-store", os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK | os.O_CLOEXEC, dir_fd=directory)
        try:
            meta = os.fstat(descriptor)
            if not stat.S_ISREG(meta.st_mode) or meta.st_uid != os.geteuid() or meta.st_mode & 0o022:
                raise ValueError("unprotected provider preference")
            data = os.read(descriptor, 65)
            provider = data.decode("ascii").strip()
            if len(data) > 64 or provider not in ("auto", "clavis", "kwallet6", "kwallet5", "kwallet", "gnome-libsecret", "none"):
                raise ValueError("invalid provider preference")
            print(provider)
        finally:
            os.close(descriptor)
    finally:
        os.close(directory)
except (OSError, ValueError, UnicodeError) as error:
    print("Cannot read Rix password provider preference: " + str(error), file=sys.stderr)
    sys.exit(1)
PY
        ) || exit 2
    fi
    secret_store="${secret_store:-auto}"
fi
if [[ "$secret_store" == auto ]]; then
    if dbus_service_available org.kde.kwalletd6.service; then
        secret_store=kwallet6
    elif dbus_service_available org.kde.kwalletd5.service; then
        secret_store=kwallet5
    elif dbus_service_available org.kde.kwalletd.service; then
        secret_store=kwallet
    elif dbus_service_available org.freedesktop.secrets.service; then
        secret_store=gnome-libsecret
    else
        secret_store=none
    fi
fi
case "$secret_store" in
    clavis)
        export XDG_CURRENT_DESKTOP=RIX:GNOME
        unset KDE_SESSION_VERSION
        if [[ "$mode" == nested && "$isolated" != 1 && "${RIX_SESSION_BUS_ISOLATED:-}" != 1 ]]; then
            echo 'Clavis requires --isolated for a nested RIX preview; the host Secret Service is preserved.' >&2
            exit 2
        fi
        clavis_session_data="$DATA_DIR/clavis/session"
        clavis_daemon="$BIN_DIR/clavis-daemon"
        if [[ ! -x "$clavis_daemon" && -f "$ROOT_DIR/Cargo.toml" ]]; then
            clavis_backend_dir="$ROOT_DIR/vendor/oo7/target/$RIX_BUILD_PROFILE"
            clavis_daemon="$clavis_backend_dir/clavis-daemon"
            export PATH="$clavis_backend_dir:$PATH"
        fi
        if [[ ! -x "$clavis_daemon" || ! -r "$clavis_session_data/dbus-1/services/org.freedesktop.secrets.service" ]]; then
            echo 'Clavis was selected but is not installed for this RIX session. Install Clavis or select the previous provider.' >&2
            exit 2
        fi
        export XDG_DATA_DIRS="$clavis_session_data:$XDG_DATA_DIRS"
        if [[ "$mode" == nested ]]; then
            export CLAVIS_STORAGE_DIR="${CLAVIS_STORAGE_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/rix/clavis-nested}"
        fi
        ;;
    kwallet6)
        export XDG_CURRENT_DESKTOP=RIX:KDE KDE_SESSION_VERSION=6 ;;
    kwallet5)
        export XDG_CURRENT_DESKTOP=RIX:KDE KDE_SESSION_VERSION=5 ;;
    kwallet)
        export XDG_CURRENT_DESKTOP=RIX:KDE KDE_SESSION_VERSION=4 ;;
    gnome-libsecret|gnome)
        export XDG_CURRENT_DESKTOP=RIX:GNOME ;;
    none)
        export XDG_CURRENT_DESKTOP=RIX ;;
    *)
        echo "Unknown RIX_SECRET_STORE '$secret_store'; use auto, clavis, kwallet6, kwallet5, kwallet, gnome-libsecret, or none." >&2
        exit 2 ;;
esac
export RIX_SECRET_STORE="$secret_store"
export XDG_SESSION_DESKTOP=RIX XDG_SESSION_TYPE=wayland
export GDK_BACKEND=wayland QT_QPA_PLATFORM=wayland SDL_VIDEODRIVER=wayland
export MOZ_GTK_TITLEBAR_DECORATION="${MOZ_GTK_TITLEBAR_DECORATION:-system}"
export ELECTRON_OZONE_PLATFORM_HINT="${ELECTRON_OZONE_PLATFORM_HINT:-auto}"
export QT_QPA_PLATFORMTHEME=xdgdesktopportal
export GTK_USE_PORTAL=1
export CLUTTER_BACKEND=wayland
export RUST_LOG="${RUST_LOG:-info}"
export RUST_BACKTRACE="${RUST_BACKTRACE:-1}"
if [[ "$rix_session_reentry" != 1 || ! "${RIX_SESSION_TOKEN:-}" =~ ^[a-fA-F0-9]{32}$ ]]; then
    unset RIX_SESSION_GENERATION RIX_RESOURCE_GENERATION
    if [[ -x "$BIN_DIR/rix-session-cleanup" ]]; then
        RIX_SESSION_TOKEN=$("$BIN_DIR/rix-session-cleanup" --new-token)
    else
        IFS= read -r RIX_SESSION_TOKEN < /proc/sys/kernel/random/uuid
        RIX_SESSION_TOKEN="${RIX_SESSION_TOKEN//-/}"
    fi
    export RIX_SESSION_TOKEN
fi
if [[ ! "$RIX_SESSION_TOKEN" =~ ^[a-fA-F0-9]{32}$ ]]; then
    echo 'Cannot create the Rix login process marker.' >&2
    exit 1
fi
if [[ "$mode" == native ]]; then
    if [[ -x "$SCRIPT_DIR/rix-doctor" ]]; then
        "$SCRIPT_DIR/rix-doctor" --required || { echo 'Native session prerequisites are missing; see rix-doctor.' >&2; exit 1; }
    fi
    export RIX_NATIVE_SESSION=1
    unset DISPLAY WAYLAND_DISPLAY WAYLAND_SOCKET
fi
if [[ "$mode" == native || "$isolated" == 1 ]] && [[ "${RIX_SESSION_BUS_ISOLATED:-}" != 1 ]]; then
    if [[ "$secret_store" == clavis ]]; then
        if [[ ! -x "$SCRIPT_DIR/rix-clavis-bus" ]]; then
            echo 'Clavis private D-Bus launcher is missing; update the RIX session helpers.' >&2
            exit 2
        fi
        exec "$SCRIPT_DIR/rix-clavis-bus" --data-dir "$clavis_session_data" -- env RIX_SESSION_REENTRY=1 RIX_SESSION_BUS_ISOLATED=1 "$0" "--$mode" "${args[@]}"
    fi
    exec dbus-run-session -- env RIX_SESSION_REENTRY=1 RIX_SESSION_BUS_ISOLATED=1 "$0" "--$mode" "${args[@]}"
fi
if [[ "$mode" == nested && -z "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ]]; then
    echo 'Nested mode requires an existing display. Use --native from a TTY.' >&2
    exit 1
fi

if [[ -x "$BIN_DIR/rix-app-compat" ]]; then
    "$BIN_DIR/rix-app-compat" --prepare-zoom || echo 'Zoom native compatibility could not be prepared; other applications remain available.' >&2
fi

clavis_pid=""
stop_clavis() {
    if [[ -n "$clavis_pid" ]]; then
        kill -TERM "$clavis_pid" 2>/dev/null || true
        local deadline=$((SECONDS + 2))
        while kill -0 "$clavis_pid" 2>/dev/null && (( SECONDS < deadline )); do sleep 0.05 || true; done
        kill -KILL "$clavis_pid" 2>/dev/null || true
        wait -f "$clavis_pid" 2>/dev/null || true
        clavis_pid=""
    fi
}
if [[ "$secret_store" == clavis ]]; then
    if ! command -v busctl >/dev/null 2>&1; then
        echo 'Clavis startup requires busctl (systemd); the desktop was not started.' >&2
        exit 2
    fi
    clavis_daemon_log="$(mktemp "${XDG_RUNTIME_DIR:-/tmp}/rix-clavis-daemon.XXXXXX")"
    "$clavis_daemon" </dev/null >"$clavis_daemon_log" 2>&1 &
    clavis_pid=$!
    trap 'stop_clavis; rm -f -- "$clavis_daemon_log"' EXIT
    clavis_deadline=$((SECONDS + 30))
    while :; do
        if ! kill -0 "$clavis_pid" 2>/dev/null; then
            if grep -q "already owns this user's storage" "$clavis_daemon_log" 2>/dev/null; then
                echo "Another Clavis daemon already owns this keyring (${CLAVIS_STORAGE_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/clavis}); stop that session or point this session at a different CLAVIS_STORAGE_DIR. The desktop was not started." >&2
            else
                echo 'Clavis exited before Secret Service became ready; the desktop was not started.' >&2
            fi
            exit 2
        fi
        if [[ "$(busctl --address="$DBUS_SESSION_BUS_ADDRESS" --timeout=1 call \
            org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus \
            NameHasOwner s org.freedesktop.secrets 2>/dev/null || true)" == 'b true' ]]; then
            if kill -0 "$clavis_pid" 2>/dev/null; then break; fi
        fi
        if (( SECONDS >= clavis_deadline )); then
            echo 'Clavis Secret Service did not become ready within 30 seconds; the desktop was not started.' >&2
            exit 2
        fi
        sleep 0.1
    done
fi

run_supervised_compositor() {
    if ! command -v setsid >/dev/null 2>&1; then
        echo 'Session isolation requires setsid (util-linux).' >&2
        return 1
    fi
    local heartbeat_timeout="${RIX_WATCHDOG_TIMEOUT_SECONDS:-8}"
    local startup_grace="${RIX_WATCHDOG_STARTUP_GRACE_SECONDS:-45}"
    local max_backoff="${RIX_WATCHDOG_MAX_BACKOFF_SECONDS:-30}"
    local pressure_grace="${RIX_WATCHDOG_PRESSURE_GRACE_SECONDS:-30}"
    local setting
    for setting in "$heartbeat_timeout" "$startup_grace" "$max_backoff" "$pressure_grace"; do
        if [[ ! "$setting" =~ ^[0-9]{1,5}$ ]]; then
            echo 'RIX_WATCHDOG_* values must be integers from 1 to 99999.' >&2
            return 2
        fi
    done
    heartbeat_timeout=$((10#$heartbeat_timeout))
    startup_grace=$((10#$startup_grace))
    max_backoff=$((10#$max_backoff))
    pressure_grace=$((10#$pressure_grace))
    if (( heartbeat_timeout < 1 || heartbeat_timeout > 300 ||
          startup_grace < heartbeat_timeout || startup_grace > 600 ||
          max_backoff < 1 || max_backoff > 300 || pressure_grace > 120 )); then
        echo 'Watchdog timeout must be 1..300s, startup grace must be timeout..600s, backoff must be 1..300s, and pressure grace must be 0..120s.' >&2
        return 2
    fi

    local runtime_dir="${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}/rixde-${UID}}"
    [[ "$runtime_dir" == /* ]] || runtime_dir="$PWD/$runtime_dir"
    local watchdog_dir="$runtime_dir/rixde-watchdog"
    local old_umask
    old_umask=$(umask)
    umask 077
    mkdir -p -- "$watchdog_dir"
    chmod 700 -- "$watchdog_dir"
    umask "$old_umask"

    local heartbeat_file="$watchdog_dir/compositor-${BASHPID}.heartbeat"
    local child_pid=""
    monotonic_now() {
        local uptime remainder
        if IFS=' ' read -r uptime remainder < /proc/uptime && [[ "$uptime" =~ ^[0-9]+\.[0-9]+$ ]]; then
            now=${uptime%%.*}
        else
            now=$SECONDS
        fi
    }
    local pressure_file=/proc/pressure/memory pressure_total=0 pressure_sample=0
    if [[ -n "${RIX_FILES_PRIVATE_FIXTURE:-}" &&
          "$HOME" == "$RIX_FILES_PRIVATE_FIXTURE/home" &&
          "$runtime_dir" == "$RIX_FILES_PRIVATE_FIXTURE/runtime" &&
          "${RIX_WATCHDOG_TEST_PRESSURE_FILE:-}" == "$RIX_FILES_PRIVATE_FIXTURE/"* ]]; then
        pressure_file="$RIX_WATCHDOG_TEST_PRESSURE_FILE"
    fi
    memory_pressure_active() {
        local line token full_average=0 total=0 elapsed delta
        if [[ ! -r "$pressure_file" ]]; then return 1; fi
        while IFS= read -r line; do
            if [[ "$line" != full\ * ]]; then continue; fi
            for token in $line; do
                if [[ "$token" =~ ^avg10=([0-9]{1,3})\.[0-9]+$ ]]; then
                    full_average=$((10#${BASH_REMATCH[1]}))
                elif [[ "$token" =~ ^total=([0-9]{1,18})$ ]]; then
                    total=$((10#${BASH_REMATCH[1]}))
                fi
            done
        done < "$pressure_file"
        elapsed=$((now - pressure_sample))
        delta=$((total - pressure_total))
        local previous_total=$pressure_total
        pressure_total=$total
        pressure_sample=$now
        (( full_average >= 5 || previous_total > 0 && elapsed > 0 && delta >= elapsed * 100000 ))
    }
    preserve_emergency_diagnostics() {
        local reason="$1" prefix="$watchdog_dir/emergency-${BASHPID}-${child_pid}"
        local data source label
        printf -v now '%(%s)T' -1
        umask 077
        printf 'pid=%s\nreason=%s\ntimestamp=%s\n' "$child_pid" "$reason" "$now" > "$prefix.meta"
        for label in compositor process-status memory-pressure; do
             case "$label" in
                 compositor) source="$heartbeat_file.diagnostics" ;;
                 process-status) source="/proc/$child_pid/status" ;;
                 memory-pressure) source="$pressure_file" ;;
             esac
             if [[ -r "$source" ]]; then
                 data=""
                 IFS= read -r -N 65536 data < "$source" || true
                 printf '%s' "$data" > "$prefix.$label"
             fi
        done
        umask "$old_umask"
        printf 'Emergency compositor snapshot: %s.meta\n' "$prefix" >&2
        local snapshots=("$watchdog_dir"/emergency-*.meta)
        local index
        for ((index=0; index<${#snapshots[@]}-10; index++)); do
            prefix="${snapshots[index]%.meta}"
            rm -f -- "$prefix.meta" "$prefix.compositor" "$prefix.process-status" "$prefix.memory-pressure"
        done
    }
    capture_compositor_diagnostics() {
        local reason="$1"
        shift
        preserve_emergency_diagnostics "$reason" || true
        if [[ -x "$SCRIPT_DIR/rix-diagnostics" ]] && command -v timeout >/dev/null 2>&1; then
            local report report_file="$watchdog_dir/report-$child_pid.path"
            (umask 077; exec timeout --kill-after=0.2s 3s "$SCRIPT_DIR/rix-diagnostics" \
                --pid "$child_pid" --reason "$reason" --heartbeat "$heartbeat_file" "$@" \
                >"$report_file") &
            local collector_pid=$! deadline=$((SECONDS + 3))
            while kill -0 "$collector_pid" 2>/dev/null && (( SECONDS < deadline )); do
                sleep 0.05 || true
            done
            if kill -0 "$collector_pid" 2>/dev/null; then
                kill -KILL -- "-$collector_pid" "$collector_pid" 2>/dev/null || true
                echo 'Compositor diagnostic collection reached its time limit; recovery will continue.' >&2
            else
                wait -f "$collector_pid" 2>/dev/null || true
            fi
            report=""
            if [[ -r "$report_file" ]]; then IFS= read -r report < "$report_file" || true; fi
            rm -f -- "$report_file"
            if [[ -n "$report" ]]; then
                printf 'Compositor failure report: %s\n' "$report" >&2
            else
                echo 'Compositor diagnostic collection failed; recovery will continue.' >&2
            fi
        fi
    }
    terminate_native_session_scope() {
        if [[ "$mode" == native && "${XDG_SESSION_ID:-}" =~ ^[A-Za-z0-9][A-Za-z0-9_-]{0,127}$ ]] && command -v loginctl >/dev/null 2>&1; then
            loginctl terminate-session "$XDG_SESSION_ID" >/dev/null 2>&1 || true
        fi
    }
    stop_session_processes() {
        if [[ -x "$BIN_DIR/rix-session-cleanup" ]] && command -v timeout >/dev/null 2>&1; then
            timeout --kill-after=0.2s 3s "$BIN_DIR/rix-session-cleanup" >/dev/null 2>&1 || true
        fi
    }
    stop_generation_processes() {
        local process_group="$1"
        if [[ "${RIX_RESOURCE_POLICY:-}" == 1 ]] && command -v systemctl >/dev/null 2>&1 && command -v timeout >/dev/null 2>&1; then
            timeout --kill-after=0.2s 2s env DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/${UID}/bus" \
                systemctl --user --no-block stop "app-rix${process_group}-foreground.scope" \
                "app-rix${process_group}-background.scope" "app-rix${process_group}.slice" >/dev/null 2>&1 || true
        fi
        if [[ -x "$BIN_DIR/rix-session-cleanup" ]] && command -v timeout >/dev/null 2>&1; then
            timeout --kill-after=0.2s 3s "$BIN_DIR/rix-session-cleanup" --generation "$process_group" >/dev/null 2>&1 || true
        fi
        kill -TERM -- "-$process_group" 2>/dev/null || true
        local deadline=$((SECONDS + 2))
        while kill -0 -- "-$process_group" 2>/dev/null && (( SECONDS < deadline )); do
            sleep 0.1 || true
        done
        kill -KILL -- "-$process_group" 2>/dev/null || true
    }
    stop_supervisor() {
        trap - TERM INT HUP
        if [[ -n "$child_pid" ]]; then
            stop_generation_processes "$child_pid"
            wait -f "$child_pid" 2>/dev/null || true
        fi
        rm -f -- "$heartbeat_file" "$heartbeat_file.diagnostics"
        stop_clavis
        stop_session_processes
        terminate_native_session_scope
        exit 0
    }
    trap stop_supervisor TERM INT HUP

    set +m
    local backoff=1 recovery_locked=0
    while true; do
        rm -f -- "$heartbeat_file" "$heartbeat_file.diagnostics"
        local started now last_heartbeat=0 heartbeat_value last_value="" previous_poll age status ended stalled=0 pressure_notice=0 under_pressure=0 threshold
        monotonic_now
        started=$now
        previous_poll=$now
        RIX_RECOVERY_LOCKED="$recovery_locked" RIX_COMPOSITOR_HEARTBEAT="$heartbeat_file" setsid /bin/sh -c \
            'export RIX_SESSION_GENERATION=$$
             if [ "${RIX_SESSION_BUS_ISOLATED:-}" = 1 ] && command -v dbus-update-activation-environment >/dev/null 2>&1; then
                 dbus-update-activation-environment RIX_SESSION_GENERATION || true
             fi
             exec "$@"' rix-session "$@" </dev/null &
        child_pid=$!

        while kill -0 "$child_pid" 2>/dev/null; do
            sleep 1
            monotonic_now
            if (( now - previous_poll > 2 )); then
                if (( last_heartbeat )); then last_heartbeat=$now; else started=$now; fi
                pressure_notice=0
            fi
            previous_poll=$now
            heartbeat_value=""
            if [[ -r "$heartbeat_file" ]]; then
                IFS= read -r heartbeat_value < "$heartbeat_file" || true
            fi
            if [[ "$heartbeat_value" =~ ^[0-9]{1,12}$ ]]; then
                if [[ "$heartbeat_value" != "$last_value" ]]; then
                    last_value=$heartbeat_value
                    last_heartbeat=$now
                    pressure_notice=0
                fi
            fi
            under_pressure=0
            if memory_pressure_active; then under_pressure=1; fi

            if (( last_heartbeat == 0 )); then
                age=$((now - started))
                threshold=$startup_grace
            else
                age=$((now - last_heartbeat))
                threshold=$heartbeat_timeout
            fi
            if (( age >= threshold )); then
                if (( under_pressure && age < threshold + pressure_grace )); then
                    if (( !pressure_notice )); then
                        echo "Kernel reports memory pressure; allowing at most ${pressure_grace}s of extra compositor recovery time." >&2
                        pressure_notice=1
                    fi
                    continue
                fi
                if (( last_heartbeat == 0 )); then
                    echo "No compositor heartbeat during startup for ${age}s; restarting the compositor." >&2
                else
                    echo "Compositor event loop stopped reporting progress for ${age}s; restarting the compositor." >&2
                fi
                stalled=1
            fi
            if (( stalled )); then
                if (( last_heartbeat == 0 )); then
                    capture_compositor_diagnostics startup-stall --age "$age"
                else
                    capture_compositor_diagnostics heartbeat-stall --age "$age"
                fi
                kill -KILL -- "-$child_pid" 2>/dev/null || true
                break
            fi
        done

        if wait -f "$child_pid"; then status=0; else status=$?; fi
        monotonic_now
        ended=$now
        if (( status != 0 && status != 78 && !stalled )); then
            capture_compositor_diagnostics unexpected-exit --exit-status "$status"
        fi
        stop_generation_processes "$child_pid"
        child_pid=""
        rm -f -- "$heartbeat_file" "$heartbeat_file.diagnostics"
        if (( status == 0 )); then
            stop_clavis
            stop_session_processes
            terminate_native_session_scope
            return 0
        fi
        if (( status == 78 )); then
            echo 'Rix session startup conflict; returning to the display manager without retrying.' >&2
            stop_clavis
            stop_session_processes
            terminate_native_session_scope
            return "$status"
        fi
        if (( ended - started >= 60 )); then backoff=1; fi
        if [[ "$mode" == native ]]; then recovery_locked=1; fi
        echo "Compositor exited with status ${status}; restarting in ${backoff}s." >&2
        sleep "$backoff"
        if (( backoff < max_backoff )); then
            backoff=$((backoff * 2))
            if (( backoff > max_backoff )); then backoff=$max_backoff; fi
        fi
    done
}

if [[ "$mode" == native ]]; then
    run_supervised_compositor "$BIN_DIR/rix-comp" --drm "${args[@]}"
    exit $?
fi
unset RIX_NATIVE_SESSION
run_supervised_compositor "$BIN_DIR/rix-comp" --nested "${args[@]}"
