#compdef thurvsa

autoload -U is-at-least

_thurvsa() {
    typeset -A opt_args
    typeset -a _arguments_options
    local ret=1

    if is-at-least 5.2; then
        _arguments_options=(-s -S -C)
    else
        _arguments_options=(-s -C)
    fi

    local context curcontext="$curcontext" state line
    _arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
'-V[Print version]' \
'--version[Print version]' \
":: :_thurvsa_commands" \
"*::: :->thurvsa" \
&& ret=0
    case $state in
    (thurvsa)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-command-$line[1]:"
        case $line[1] in
            (volume)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvsa__subcmd__volume_commands" \
"*::: :->volume" \
&& ret=0

    case $state in
    (volume)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-volume-command-$line[1]:"
        case $line[1] in
            (create)
_arguments "${_arguments_options[@]}" : \
'--size=[Logical volume size, e.g. \`1T\`, \`500G\`, \`4096\`]:SIZE:_default' \
'--backend=[Storage backend name to bind this volume to]:BACKEND:_default' \
'--page-size=[Page size — chunk unit for backend upload + disk cache]:PAGE_SIZE:_default' \
'--dedup=[Dedup scope\: \`global\` (default) or \`local\`]:DEDUP:(local global)' \
'--key-file=[Supply the at-rest DEK from PATH instead of minting one]:PATH:_files' \
'--keystore=[Keystore backend that wraps this volume'\''s DEK]:NAME:_default' \
'--dek-source=[Where the DEK is minted (requires --encrypt)]:DEK_SOURCE:(daemon backend)' \
'--sync-after=[Initial SYNCHRONIZE CACHE durability tier (mutable later via \`volume modify --sync-after\`)]:SYNC_AFTER:(storage disk memory)' \
'--lun=[Pin this volume to LUN N]:N:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--worm[Mark the volume Write-Once-Read-Many]' \
'--encrypt[Enable at-rest encryption (requires --keystore)]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Volume name (1-64 chars\: letters, digits, '\''-'\'', '\''_'\''):_default' \
&& ret=0
;;
(list)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the response as JSON for automation]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(info)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the manifest as JSON for automation]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Volume name:_default' \
&& ret=0
;;
(destroy)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--force[Confirm destruction. Without this flag the command refuses; the volume contents are gone after a destroy]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Volume name:_default' \
&& ret=0
;;
(modify)
_arguments "${_arguments_options[@]}" : \
'--sync-after=[New SYNCHRONIZE CACHE durability tier]:SYNC_AFTER:(storage disk memory)' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Volume name:_default' \
&& ret=0
;;
(resize)
_arguments "${_arguments_options[@]}" : \
'--size=[New logical volume size, e.g. \`2T\`, \`500G\`, \`8192\`]:SIZE:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Volume name:_default' \
&& ret=0
;;
(key)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvsa__subcmd__volume__subcmd__key_commands" \
"*::: :->key" \
&& ret=0

    case $state in
    (key)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-volume-key-command-$line[1]:"
        case $line[1] in
            (migrate)
_arguments "${_arguments_options[@]}" : \
'--to=[New keystore backend name]:TO:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--purge-local[Delete the local-keystore sidecar after migrating away]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Volume name:_default' \
&& ret=0
;;
(export)
_arguments "${_arguments_options[@]}" : \
'--to=[Output file path. Refuses if the file already exists]:TO:_files' \
'--iter=[PBKDF2 iteration count. Default tuned for ~1 s on modern x86_64; raise for stronger work factor at the cost of slower export/import. Mirrors \`shared_keystore\:\:passphrase_envelope\:\:DEFAULT_P2C\`]:ITER:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Volume name:_default' \
&& ret=0
;;
(import)
_arguments "${_arguments_options[@]}" : \
'--from=[JWE Compact envelope file path (produced by \`volume key export\`)]:FROM:_files' \
'--keystore=[Target keystore backend name to rewrap into. Optional if \`keystore.backends\:\` defines exactly one entry]:KEYSTORE:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Volume name:_default' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__volume__subcmd__key__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-volume-key-help-command-$line[1]:"
        case $line[1] in
            (migrate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(export)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(import)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__volume__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-volume-help-command-$line[1]:"
        case $line[1] in
            (create)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(info)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(destroy)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(modify)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(resize)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(key)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__volume__subcmd__help__subcmd__key_commands" \
"*::: :->key" \
&& ret=0

    case $state in
    (key)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-volume-help-key-command-$line[1]:"
        case $line[1] in
            (migrate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(export)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(import)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(system)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvsa__subcmd__system_commands" \
"*::: :->system" \
&& ret=0

    case $state in
    (system)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-system-command-$line[1]:"
        case $line[1] in
            (storage)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvsa__subcmd__system__subcmd__storage_commands" \
"*::: :->storage" \
&& ret=0

    case $state in
    (storage)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-system-storage-command-$line[1]:"
        case $line[1] in
            (check)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(benchmark)
_arguments "${_arguments_options[@]}" : \
'*--backend=[Backend name to benchmark. Repeatable]:BACKENDS:_default' \
'--total-gb=[GiB per cell. Default 32]:TOTAL_GB:_default' \
'--chunk-size-mb=[MiB per upload. Default 8 matches the FastCDC chunk average]:CHUNK_SIZE_MB:_default' \
'--concurrency=[Parallel in-flight uploads per cell. Default 16]:CONCURRENCY:_default' \
'*--chunk-size-mb-sweep=[Sweep chunk size across this comma-separated list]:CHUNK_SIZE_MB_SWEEP:_default' \
'*--concurrency-sweep=[Sweep concurrency across this comma-separated list]:CONCURRENCY_SWEEP:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--skip-download[Skip the download phase]' \
'--yes[Bypass the sweep-preview prompt (scripted runs)]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__system__subcmd__storage__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-system-storage-help-command-$line[1]:"
        case $line[1] in
            (check)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(benchmark)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(gc)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--dry-run[Show what would be deleted without actually deleting]' \
'--storage[Also delete orphan objects from the storage backend]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(regenerate-cert)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(alerting)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvsa__subcmd__system__subcmd__alerting_commands" \
"*::: :->alerting" \
&& ret=0

    case $state in
    (alerting)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-system-alerting-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(test)
_arguments "${_arguments_options[@]}" : \
'--severity=[Severity tag on the synthetic alert]:SEVERITY:(info warn error)' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':sink -- Sink name from the YAML `alerting.sinks` list:_default' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__system__subcmd__alerting__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-system-alerting-help-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(test)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(daemon-health)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the response as JSON for automation]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(monitor)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(audit)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvsa__subcmd__system__subcmd__audit_commands" \
"*::: :->audit" \
&& ret=0

    case $state in
    (audit)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-system-audit-command-$line[1]:"
        case $line[1] in
            (tail)
_arguments "${_arguments_options[@]}" : \
'-n+[Number of trailing entries before follow mode (default 20)]:LINES:_default' \
'--lines=[Number of trailing entries before follow mode (default 20)]:LINES:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-f[Follow new entries as they land]' \
'--follow[Follow new entries as they land]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(export)
_arguments "${_arguments_options[@]}" : \
'--format=[Output format\: \`jsonl\` or \`csv\`]:FORMAT:(jsonl csv)' \
'--from=[Inclusive start date (YYYY-MM-DD)]:FROM:_default' \
'--to=[Inclusive end date (YYYY-MM-DD)]:TO:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(verify)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(verify-offline)
_arguments "${_arguments_options[@]}" : \
'--dir=[Path to the audit directory to verify (typically the \`audit/\` subdirectory of a \`data_dir\` copy)]:DIR:_files' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the VerifyReport as JSON for automation]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--accept-break[Required confirmation. Without this flag, refuses to run]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__system__subcmd__audit__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-system-audit-help-command-$line[1]:"
        case $line[1] in
            (tail)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(export)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(verify)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(verify-offline)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(stats)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the full report as JSON for automation]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(verify)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--skip-storage[Skip the storage-backend sweep (local-only audit)]' \
'--verbose[Per-volume breakdown (every error and warning)]' \
'--json[Emit the full report as JSON for CI / automation]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
'*::volumes -- Optional volume names to limit the sweep:_default' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__system__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-system-help-command-$line[1]:"
        case $line[1] in
            (storage)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__system__subcmd__help__subcmd__storage_commands" \
"*::: :->storage" \
&& ret=0

    case $state in
    (storage)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-system-help-storage-command-$line[1]:"
        case $line[1] in
            (check)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(benchmark)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(gc)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(regenerate-cert)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(alerting)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__system__subcmd__help__subcmd__alerting_commands" \
"*::: :->alerting" \
&& ret=0

    case $state in
    (alerting)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-system-help-alerting-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(test)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(daemon-health)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(monitor)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(audit)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__system__subcmd__help__subcmd__audit_commands" \
"*::: :->audit" \
&& ret=0

    case $state in
    (audit)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-system-help-audit-command-$line[1]:"
        case $line[1] in
            (tail)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(export)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(verify)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(verify-offline)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(stats)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(verify)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(iscsi)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvsa__subcmd__iscsi_commands" \
"*::: :->iscsi" \
&& ret=0

    case $state in
    (iscsi)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-iscsi-command-$line[1]:"
        case $line[1] in
            (users)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvsa__subcmd__iscsi__subcmd__users_commands" \
"*::: :->users" \
&& ret=0

    case $state in
    (users)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-iscsi-users-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit JSON for automation]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(add)
_arguments "${_arguments_options[@]}" : \
'(--password-stdin)--password=[Password as a CLI arg. Mutually exclusive with \`--password-stdin\`]:PASSWORD:_default' \
'--partition=[Partition the user is fenced to (VTL only; VSA ignores)]:PARTITION:_default' \
'*--volume=[Volume the user is admitted to (repeatable, required)]:NAME:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--password-stdin[Read the password from stdin (single line, trailing newline stripped). Mutually exclusive with \`--password\`]' \
'--mutual-chap[Enable mutual CHAP (target authenticates back to the initiator). Requires \`iscsi target set\` to have been run]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Username (CHAP identity the initiator presents):_default' \
&& ret=0
;;
(grant)
_arguments "${_arguments_options[@]}" : \
'*--volume=[Volume to add to the user'\''s allow-list (repeatable, required)]:NAME:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Username to grant access to:_default' \
&& ret=0
;;
(revoke)
_arguments "${_arguments_options[@]}" : \
'*--volume=[Volume to remove from the user'\''s allow-list (repeatable, required)]:NAME:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Username to revoke access from:_default' \
&& ret=0
;;
(remove)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Username to remove:_default' \
&& ret=0
;;
(disable)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Username to disable:_default' \
&& ret=0
;;
(enable)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Username to re-enable:_default' \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
'(--password-stdin --cancel)--password=[New password as a CLI arg. Mutually exclusive with \`--password-stdin\`]:PASSWORD:_default' \
'(--cancel)--grace=[Grace window during which both passwords remain valid. Humantime\: \`24h\`, \`5m\`, \`1d12h\`. Default \`24h\`]:GRACE:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'(--cancel)--password-stdin[Read the new password from stdin (single line)]' \
'--cancel[Cancel an in-flight rotation\: drop the new password, restore the previous one as sole current. Errors if no rotation is in progress]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Username to rotate:_default' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-iscsi-users-help-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(add)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(grant)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(revoke)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(remove)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(disable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(enable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(target)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvsa__subcmd__iscsi__subcmd__target_commands" \
"*::: :->target" \
&& ret=0

    case $state in
    (target)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-iscsi-target-command-$line[1]:"
        case $line[1] in
            (show)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit JSON for automation]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(set)
_arguments "${_arguments_options[@]}" : \
'--username=[Target username (sent in \`CHAP_N\` during mutual auth)]:USERNAME:_default' \
'(--password-stdin)--password=[Password as a CLI arg. Mutually exclusive with \`--password-stdin\`]:PASSWORD:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--password-stdin[Read the password from stdin (single line)]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(clear)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__iscsi__subcmd__target__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-iscsi-target-help-command-$line[1]:"
        case $line[1] in
            (show)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(set)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(clear)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__iscsi__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-iscsi-help-command-$line[1]:"
        case $line[1] in
            (users)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users_commands" \
"*::: :->users" \
&& ret=0

    case $state in
    (users)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-iscsi-help-users-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(add)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(grant)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(revoke)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(remove)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(disable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(enable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(target)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__iscsi__subcmd__help__subcmd__target_commands" \
"*::: :->target" \
&& ret=0

    case $state in
    (target)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-iscsi-help-target-command-$line[1]:"
        case $line[1] in
            (show)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(set)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(clear)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(nvmetcp)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvsa__subcmd__nvmetcp_commands" \
"*::: :->nvmetcp" \
&& ret=0

    case $state in
    (nvmetcp)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-nvmetcp-command-$line[1]:"
        case $line[1] in
            (psks)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvsa__subcmd__nvmetcp__subcmd__psks_commands" \
"*::: :->psks" \
&& ret=0

    case $state in
    (psks)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-nvmetcp-psks-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit JSON for automation]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(add)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Initiator host NQN (must match \`nvme connect --hostnqn\`)]:HOST_NQN:_default' \
'--key=[\`NVMeTLSkey-X\:NN\:base64\:\` interchange string]:KEY:_default' \
'*--volume=[Volume the host is admitted to (repeatable, required)]:NAME:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(grant)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN to grant access to]:HOST_NQN:_default' \
'*--volume=[Volume to add to the host'\''s allow-list (repeatable, required)]:NAME:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(revoke)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN to revoke access from]:HOST_NQN:_default' \
'*--volume=[Volume to remove from the host'\''s allow-list (repeatable, required)]:NAME:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(remove)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN to remove]:HOST_NQN:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(disable)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN to disable]:HOST_NQN:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(enable)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN to re-enable]:HOST_NQN:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN to rotate]:HOST_NQN:_default' \
'(--cancel)--key=[New \`NVMeTLSkey-...\` interchange string]:KEY:_default' \
'(--cancel)--grace=[Grace window (humantime\: \`24h\`, \`5m\`, \`1d12h\`). Default \`24h\`]:GRACE:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--cancel[Cancel an in-flight rotation\: drop the new key, restore the previous one. Errors if no rotation is in progress]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-nvmetcp-psks-help-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(add)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(grant)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(revoke)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(remove)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(disable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(enable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(dhchap)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvsa__subcmd__nvmetcp__subcmd__dhchap_commands" \
"*::: :->dhchap" \
&& ret=0

    case $state in
    (dhchap)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-nvmetcp-dhchap-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit JSON for automation]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(add)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Initiator host NQN (must match \`nvme connect --hostnqn\`)]:HOST_NQN:_default' \
'--key=[\`DHHC-1\:NN\:base64\:\` host secret]:KEY:_default' \
'--ctrl-key=[Optional \`DHHC-1\:...\` controller secret (mutual auth)]:CTRL_KEY:_default' \
'*--volume=[Volume the host is admitted to (repeatable, required)]:NAME:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(grant)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN to grant access to]:HOST_NQN:_default' \
'*--volume=[Volume to add to the host'\''s allow-list (repeatable, required)]:NAME:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(revoke)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN to revoke access from]:HOST_NQN:_default' \
'*--volume=[Volume to remove from the host'\''s allow-list (repeatable, required)]:NAME:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(remove)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN to remove]:HOST_NQN:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(disable)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN to disable]:HOST_NQN:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(enable)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN to re-enable]:HOST_NQN:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(set-ctrl-key)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN]:HOST_NQN:_default' \
'--key=[\`DHHC-1\:...\` controller secret]:KEY:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(clear-ctrl-key)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN]:HOST_NQN:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
'--host-nqn=[Host NQN to rotate]:HOST_NQN:_default' \
'(--cancel)--key=[New \`DHHC-1\:...\` secret]:KEY:_default' \
'(--cancel)--grace=[Grace window (humantime\: \`24h\`, \`5m\`, \`1d12h\`). Default \`24h\`]:GRACE:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--cancel[Cancel an in-flight rotation\: drop the new secret, restore the previous one. Errors if no rotation is in progress]' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-nvmetcp-dhchap-help-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(add)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(grant)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(revoke)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(remove)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(disable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(enable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(set-ctrl-key)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(clear-ctrl-key)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__nvmetcp__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-nvmetcp-help-command-$line[1]:"
        case $line[1] in
            (psks)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks_commands" \
"*::: :->psks" \
&& ret=0

    case $state in
    (psks)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-nvmetcp-help-psks-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(add)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(grant)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(revoke)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(remove)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(disable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(enable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(dhchap)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap_commands" \
"*::: :->dhchap" \
&& ret=0

    case $state in
    (dhchap)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-nvmetcp-help-dhchap-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(add)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(grant)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(revoke)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(remove)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(disable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(enable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(set-ctrl-key)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(clear-ctrl-key)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(config)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvsa__subcmd__config_commands" \
"*::: :->config" \
&& ret=0

    case $state in
    (config)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-config-command-$line[1]:"
        case $line[1] in
            (defaults)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(systemd-unit)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(completion)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--copyright[Print the copyright + license notice and exit]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
'::shell -- Target shell. Defaults to `basename $SHELL`:(bash elvish fish powershell zsh)' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__config__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-config-help-command-$line[1]:"
        case $line[1] in
            (defaults)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(systemd-unit)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(completion)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-command-$line[1]:"
        case $line[1] in
            (volume)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help__subcmd__volume_commands" \
"*::: :->volume" \
&& ret=0

    case $state in
    (volume)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-volume-command-$line[1]:"
        case $line[1] in
            (create)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(info)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(destroy)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(modify)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(resize)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(key)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help__subcmd__volume__subcmd__key_commands" \
"*::: :->key" \
&& ret=0

    case $state in
    (key)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-volume-key-command-$line[1]:"
        case $line[1] in
            (migrate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(export)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(import)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(system)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help__subcmd__system_commands" \
"*::: :->system" \
&& ret=0

    case $state in
    (system)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-system-command-$line[1]:"
        case $line[1] in
            (storage)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help__subcmd__system__subcmd__storage_commands" \
"*::: :->storage" \
&& ret=0

    case $state in
    (storage)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-system-storage-command-$line[1]:"
        case $line[1] in
            (check)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(benchmark)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(gc)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(regenerate-cert)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(alerting)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help__subcmd__system__subcmd__alerting_commands" \
"*::: :->alerting" \
&& ret=0

    case $state in
    (alerting)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-system-alerting-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(test)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(daemon-health)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(monitor)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(audit)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help__subcmd__system__subcmd__audit_commands" \
"*::: :->audit" \
&& ret=0

    case $state in
    (audit)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-system-audit-command-$line[1]:"
        case $line[1] in
            (tail)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(export)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(verify)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(verify-offline)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(stats)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(verify)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(iscsi)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help__subcmd__iscsi_commands" \
"*::: :->iscsi" \
&& ret=0

    case $state in
    (iscsi)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-iscsi-command-$line[1]:"
        case $line[1] in
            (users)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users_commands" \
"*::: :->users" \
&& ret=0

    case $state in
    (users)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-iscsi-users-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(add)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(grant)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(revoke)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(remove)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(disable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(enable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(target)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help__subcmd__iscsi__subcmd__target_commands" \
"*::: :->target" \
&& ret=0

    case $state in
    (target)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-iscsi-target-command-$line[1]:"
        case $line[1] in
            (show)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(set)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(clear)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(nvmetcp)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help__subcmd__nvmetcp_commands" \
"*::: :->nvmetcp" \
&& ret=0

    case $state in
    (nvmetcp)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-nvmetcp-command-$line[1]:"
        case $line[1] in
            (psks)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks_commands" \
"*::: :->psks" \
&& ret=0

    case $state in
    (psks)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-nvmetcp-psks-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(add)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(grant)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(revoke)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(remove)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(disable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(enable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(dhchap)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap_commands" \
"*::: :->dhchap" \
&& ret=0

    case $state in
    (dhchap)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-nvmetcp-dhchap-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(add)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(grant)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(revoke)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(remove)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(disable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(enable)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(set-ctrl-key)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(clear-ctrl-key)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
;;
(config)
_arguments "${_arguments_options[@]}" : \
":: :_thurvsa__subcmd__help__subcmd__config_commands" \
"*::: :->config" \
&& ret=0

    case $state in
    (config)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvsa-help-config-command-$line[1]:"
        case $line[1] in
            (defaults)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(systemd-unit)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(completion)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(help)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
        esac
    ;;
esac
}

(( $+functions[_thurvsa_commands] )) ||
_thurvsa_commands() {
    local commands; commands=(
'volume:Volume management (create, list, info, destroy)' \
'system:System operations' \
'iscsi:iSCSI CHAP credentials' \
'nvmetcp:NVMe-TCP TLS-PSK credentials' \
'config:Configuration helpers (defaults yaml, systemd unit, shell completion)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__config_commands] )) ||
_thurvsa__subcmd__config_commands() {
    local commands; commands=(
'defaults:Emit the default configuration yaml on stdout' \
'systemd-unit:Emit the default systemd unit file on stdout' \
'completion:Emit a shell completion script on stdout' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa config commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__config__subcmd__completion_commands] )) ||
_thurvsa__subcmd__config__subcmd__completion_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa config completion commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__config__subcmd__defaults_commands] )) ||
_thurvsa__subcmd__config__subcmd__defaults_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa config defaults commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__config__subcmd__help_commands] )) ||
_thurvsa__subcmd__config__subcmd__help_commands() {
    local commands; commands=(
'defaults:Emit the default configuration yaml on stdout' \
'systemd-unit:Emit the default systemd unit file on stdout' \
'completion:Emit a shell completion script on stdout' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa config help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__config__subcmd__help__subcmd__completion_commands] )) ||
_thurvsa__subcmd__config__subcmd__help__subcmd__completion_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa config help completion commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__config__subcmd__help__subcmd__defaults_commands] )) ||
_thurvsa__subcmd__config__subcmd__help__subcmd__defaults_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa config help defaults commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__config__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__config__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa config help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__config__subcmd__help__subcmd__systemd-unit_commands] )) ||
_thurvsa__subcmd__config__subcmd__help__subcmd__systemd-unit_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa config help systemd-unit commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__config__subcmd__systemd-unit_commands] )) ||
_thurvsa__subcmd__config__subcmd__systemd-unit_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa config systemd-unit commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help_commands] )) ||
_thurvsa__subcmd__help_commands() {
    local commands; commands=(
'volume:Volume management (create, list, info, destroy)' \
'system:System operations' \
'iscsi:iSCSI CHAP credentials' \
'nvmetcp:NVMe-TCP TLS-PSK credentials' \
'config:Configuration helpers (defaults yaml, systemd unit, shell completion)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__config_commands] )) ||
_thurvsa__subcmd__help__subcmd__config_commands() {
    local commands; commands=(
'defaults:Emit the default configuration yaml on stdout' \
'systemd-unit:Emit the default systemd unit file on stdout' \
'completion:Emit a shell completion script on stdout' \
    )
    _describe -t commands 'thurvsa help config commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__config__subcmd__completion_commands] )) ||
_thurvsa__subcmd__help__subcmd__config__subcmd__completion_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help config completion commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__config__subcmd__defaults_commands] )) ||
_thurvsa__subcmd__help__subcmd__config__subcmd__defaults_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help config defaults commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__config__subcmd__systemd-unit_commands] )) ||
_thurvsa__subcmd__help__subcmd__config__subcmd__systemd-unit_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help config systemd-unit commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi_commands() {
    local commands; commands=(
'users:CHAP user lifecycle' \
'target:Mutual-CHAP target credential' \
    )
    _describe -t commands 'thurvsa help iscsi commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi__subcmd__target_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi__subcmd__target_commands() {
    local commands; commands=(
'show:Show the current target identity (password value hidden)' \
'set:Set both target_username and target_password' \
'clear:Clear both target_username and target_password' \
    )
    _describe -t commands 'thurvsa help iscsi target commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__clear_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__clear_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help iscsi target clear commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__set_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__set_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help iscsi target set commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__show_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help iscsi target show commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users_commands() {
    local commands; commands=(
'list:List every CHAP user' \
'add:Add a new CHAP user' \
'grant:Grant a user access to one or more volumes' \
'revoke:Revoke a user'\''s access to one or more volumes' \
'remove:Remove a CHAP user' \
'disable:Disable a user without removing the entry' \
'enable:Re-enable a previously disabled user' \
'rotate:Rotate a user'\''s password with a grace window' \
    )
    _describe -t commands 'thurvsa help iscsi users commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__add_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help iscsi users add commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__disable_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help iscsi users disable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__enable_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help iscsi users enable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__grant_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__grant_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help iscsi users grant commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__list_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help iscsi users list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__remove_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help iscsi users remove commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__revoke_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__revoke_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help iscsi users revoke commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help iscsi users rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp_commands() {
    local commands; commands=(
'psks:TLS-PSK lifecycle for NVMe-TCP hosts' \
'dhchap:DH-HMAC-CHAP in-band auth lifecycle for NVMe-TCP hosts' \
    )
    _describe -t commands 'thurvsa help nvmetcp commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap_commands() {
    local commands; commands=(
'list:List every registered host DH-HMAC-CHAP entry' \
'add:Add a new host DH-HMAC-CHAP secret' \
'grant:Grant a host access to one or more volumes' \
'revoke:Revoke a host'\''s access to one or more volumes' \
'remove:Remove a host DH-HMAC-CHAP entry' \
'disable:Disable a host entry without removing it' \
'enable:Re-enable a previously disabled host entry' \
'set-ctrl-key:Set (or replace) a host'\''s controller secret for mutual auth' \
'clear-ctrl-key:Clear a host'\''s controller secret (disable mutual auth)' \
'rotate:Rotate a host'\''s DH-HMAC-CHAP secret with a grace window' \
    )
    _describe -t commands 'thurvsa help nvmetcp dhchap commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__add_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp dhchap add commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__clear-ctrl-key_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__clear-ctrl-key_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp dhchap clear-ctrl-key commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__disable_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp dhchap disable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__enable_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp dhchap enable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__grant_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__grant_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp dhchap grant commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__list_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp dhchap list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__remove_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp dhchap remove commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__revoke_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__revoke_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp dhchap revoke commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp dhchap rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__set-ctrl-key_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__dhchap__subcmd__set-ctrl-key_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp dhchap set-ctrl-key commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks_commands() {
    local commands; commands=(
'list:List every registered host PSK' \
'add:Add a new host PSK' \
'grant:Grant a host access to one or more volumes' \
'revoke:Revoke a host'\''s access to one or more volumes' \
'remove:Remove a host PSK' \
'disable:Disable a host PSK without removing the entry' \
'enable:Re-enable a previously disabled host PSK' \
'rotate:Rotate a host'\''s PSK with a grace window' \
    )
    _describe -t commands 'thurvsa help nvmetcp psks commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__add_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp psks add commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__disable_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp psks disable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__enable_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp psks enable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__grant_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__grant_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp psks grant commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__list_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp psks list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__remove_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp psks remove commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__revoke_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__revoke_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp psks revoke commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help nvmetcp psks rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system_commands] )) ||
_thurvsa__subcmd__help__subcmd__system_commands() {
    local commands; commands=(
'storage:Storage-backend operations' \
'gc:Garbage-collect orphan chunks from the chunk pool' \
'regenerate-cert:Regenerate the admin HTTP self-signed TLS cert' \
'alerting:First-party alerting (email + webhook)' \
'daemon-health:Probe the daemon'\''s admin Unix socket' \
'monitor:Live activity screen — holds and redraws ~1s, Ctrl-C to exit' \
'audit:Audit-chain operations' \
'stats:Dedup ratio and per-volume contribution' \
'verify:Volume-wide consistency check' \
    )
    _describe -t commands 'thurvsa help system commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__alerting_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__alerting_commands() {
    local commands; commands=(
'list:Show configured alert sinks and dedup window' \
'test:Fire a synthetic alert through one sink' \
    )
    _describe -t commands 'thurvsa help system alerting commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__alerting__subcmd__list_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__alerting__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system alerting list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__alerting__subcmd__test_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__alerting__subcmd__test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system alerting test commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__audit_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__audit_commands() {
    local commands; commands=(
'tail:Print recent audit entries (optionally follow with -f)' \
'export:Export entries in the requested date range' \
'verify:Verify the tamper-evident chain end-to-end' \
'verify-offline:Offline-verify a copy of an audit directory' \
'rotate:Operator-acknowledged chain reset after a verify failure' \
    )
    _describe -t commands 'thurvsa help system audit commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__export_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system audit export commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system audit rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__tail_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__tail_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system audit tail commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system audit verify commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify-offline_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify-offline_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system audit verify-offline commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__daemon-health_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__daemon-health_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system daemon-health commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__gc_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__gc_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system gc commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__monitor_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__monitor_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system monitor commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__regenerate-cert_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__regenerate-cert_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system regenerate-cert commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__stats_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system stats commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__storage_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__storage_commands() {
    local commands; commands=(
'check:Check storage-backend connectivity, auth, and read/write/delete' \
'benchmark:First-party storage-backend throughput benchmark (daemon-down)' \
    )
    _describe -t commands 'thurvsa help system storage commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__storage__subcmd__benchmark_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__storage__subcmd__benchmark_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system storage benchmark commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__storage__subcmd__check_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__storage__subcmd__check_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system storage check commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__system__subcmd__verify_commands] )) ||
_thurvsa__subcmd__help__subcmd__system__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help system verify commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__volume_commands] )) ||
_thurvsa__subcmd__help__subcmd__volume_commands() {
    local commands; commands=(
'create:Create a new volume' \
'list:List every volume' \
'info:Show one volume'\''s manifest' \
'destroy:Destroy a volume' \
'modify:Modify a live volume'\''s mutable settings' \
'resize:Grow a volume'\''s capacity' \
'key:Per-volume key-management operations' \
    )
    _describe -t commands 'thurvsa help volume commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__volume__subcmd__create_commands] )) ||
_thurvsa__subcmd__help__subcmd__volume__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help volume create commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__volume__subcmd__destroy_commands] )) ||
_thurvsa__subcmd__help__subcmd__volume__subcmd__destroy_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help volume destroy commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__volume__subcmd__info_commands] )) ||
_thurvsa__subcmd__help__subcmd__volume__subcmd__info_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help volume info commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__volume__subcmd__key_commands] )) ||
_thurvsa__subcmd__help__subcmd__volume__subcmd__key_commands() {
    local commands; commands=(
'migrate:Move a volume'\''s DEK to a different keystore backend' \
'export:Export a volume'\''s DEK to a passphrase-sealed file (JWE/PBES2)' \
'import:Import a passphrase-sealed DEK envelope into a volume' \
    )
    _describe -t commands 'thurvsa help volume key commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__volume__subcmd__key__subcmd__export_commands] )) ||
_thurvsa__subcmd__help__subcmd__volume__subcmd__key__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help volume key export commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__volume__subcmd__key__subcmd__import_commands] )) ||
_thurvsa__subcmd__help__subcmd__volume__subcmd__key__subcmd__import_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help volume key import commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__volume__subcmd__key__subcmd__migrate_commands] )) ||
_thurvsa__subcmd__help__subcmd__volume__subcmd__key__subcmd__migrate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help volume key migrate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__volume__subcmd__list_commands] )) ||
_thurvsa__subcmd__help__subcmd__volume__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help volume list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__volume__subcmd__modify_commands] )) ||
_thurvsa__subcmd__help__subcmd__volume__subcmd__modify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help volume modify commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__help__subcmd__volume__subcmd__resize_commands] )) ||
_thurvsa__subcmd__help__subcmd__volume__subcmd__resize_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa help volume resize commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi_commands] )) ||
_thurvsa__subcmd__iscsi_commands() {
    local commands; commands=(
'users:CHAP user lifecycle' \
'target:Mutual-CHAP target credential' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa iscsi commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help_commands() {
    local commands; commands=(
'users:CHAP user lifecycle' \
'target:Mutual-CHAP target credential' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa iscsi help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__target_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__target_commands() {
    local commands; commands=(
'show:Show the current target identity (password value hidden)' \
'set:Set both target_username and target_password' \
'clear:Clear both target_username and target_password' \
    )
    _describe -t commands 'thurvsa iscsi help target commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__clear_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__clear_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi help target clear commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__set_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__set_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi help target set commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__show_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi help target show commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users_commands() {
    local commands; commands=(
'list:List every CHAP user' \
'add:Add a new CHAP user' \
'grant:Grant a user access to one or more volumes' \
'revoke:Revoke a user'\''s access to one or more volumes' \
'remove:Remove a CHAP user' \
'disable:Disable a user without removing the entry' \
'enable:Re-enable a previously disabled user' \
'rotate:Rotate a user'\''s password with a grace window' \
    )
    _describe -t commands 'thurvsa iscsi help users commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__add_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi help users add commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__disable_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi help users disable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__enable_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi help users enable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__grant_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__grant_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi help users grant commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__list_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi help users list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__remove_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi help users remove commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__revoke_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__revoke_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi help users revoke commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi help users rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__target_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__target_commands() {
    local commands; commands=(
'show:Show the current target identity (password value hidden)' \
'set:Set both target_username and target_password' \
'clear:Clear both target_username and target_password' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa iscsi target commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__target__subcmd__clear_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__target__subcmd__clear_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi target clear commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__target__subcmd__help_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__target__subcmd__help_commands() {
    local commands; commands=(
'show:Show the current target identity (password value hidden)' \
'set:Set both target_username and target_password' \
'clear:Clear both target_username and target_password' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa iscsi target help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__clear_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__clear_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi target help clear commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi target help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__set_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__set_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi target help set commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__show_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi target help show commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__target__subcmd__set_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__target__subcmd__set_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi target set commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__target__subcmd__show_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__target__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi target show commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users_commands() {
    local commands; commands=(
'list:List every CHAP user' \
'add:Add a new CHAP user' \
'grant:Grant a user access to one or more volumes' \
'revoke:Revoke a user'\''s access to one or more volumes' \
'remove:Remove a CHAP user' \
'disable:Disable a user without removing the entry' \
'enable:Re-enable a previously disabled user' \
'rotate:Rotate a user'\''s password with a grace window' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa iscsi users commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__add_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users add commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__disable_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users disable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__enable_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users enable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__grant_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__grant_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users grant commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help_commands() {
    local commands; commands=(
'list:List every CHAP user' \
'add:Add a new CHAP user' \
'grant:Grant a user access to one or more volumes' \
'revoke:Revoke a user'\''s access to one or more volumes' \
'remove:Remove a CHAP user' \
'disable:Disable a user without removing the entry' \
'enable:Re-enable a previously disabled user' \
'rotate:Rotate a user'\''s password with a grace window' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa iscsi users help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__add_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users help add commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__disable_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users help disable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__enable_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users help enable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__grant_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__grant_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users help grant commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__list_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users help list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__remove_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users help remove commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__revoke_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__revoke_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users help revoke commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users help rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__list_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__remove_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users remove commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__revoke_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__revoke_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users revoke commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__iscsi__subcmd__users__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__iscsi__subcmd__users__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa iscsi users rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp_commands] )) ||
_thurvsa__subcmd__nvmetcp_commands() {
    local commands; commands=(
'psks:TLS-PSK lifecycle for NVMe-TCP hosts' \
'dhchap:DH-HMAC-CHAP in-band auth lifecycle for NVMe-TCP hosts' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa nvmetcp commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap_commands() {
    local commands; commands=(
'list:List every registered host DH-HMAC-CHAP entry' \
'add:Add a new host DH-HMAC-CHAP secret' \
'grant:Grant a host access to one or more volumes' \
'revoke:Revoke a host'\''s access to one or more volumes' \
'remove:Remove a host DH-HMAC-CHAP entry' \
'disable:Disable a host entry without removing it' \
'enable:Re-enable a previously disabled host entry' \
'set-ctrl-key:Set (or replace) a host'\''s controller secret for mutual auth' \
'clear-ctrl-key:Clear a host'\''s controller secret (disable mutual auth)' \
'rotate:Rotate a host'\''s DH-HMAC-CHAP secret with a grace window' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa nvmetcp dhchap commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__add_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap add commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__clear-ctrl-key_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__clear-ctrl-key_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap clear-ctrl-key commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__disable_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap disable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__enable_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap enable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__grant_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__grant_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap grant commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help_commands() {
    local commands; commands=(
'list:List every registered host DH-HMAC-CHAP entry' \
'add:Add a new host DH-HMAC-CHAP secret' \
'grant:Grant a host access to one or more volumes' \
'revoke:Revoke a host'\''s access to one or more volumes' \
'remove:Remove a host DH-HMAC-CHAP entry' \
'disable:Disable a host entry without removing it' \
'enable:Re-enable a previously disabled host entry' \
'set-ctrl-key:Set (or replace) a host'\''s controller secret for mutual auth' \
'clear-ctrl-key:Clear a host'\''s controller secret (disable mutual auth)' \
'rotate:Rotate a host'\''s DH-HMAC-CHAP secret with a grace window' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa nvmetcp dhchap help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__add_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap help add commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__clear-ctrl-key_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__clear-ctrl-key_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap help clear-ctrl-key commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__disable_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap help disable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__enable_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap help enable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__grant_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__grant_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap help grant commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__list_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap help list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__remove_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap help remove commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__revoke_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__revoke_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap help revoke commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap help rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__set-ctrl-key_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__help__subcmd__set-ctrl-key_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap help set-ctrl-key commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__list_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__remove_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap remove commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__revoke_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__revoke_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap revoke commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__set-ctrl-key_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__dhchap__subcmd__set-ctrl-key_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp dhchap set-ctrl-key commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help_commands() {
    local commands; commands=(
'psks:TLS-PSK lifecycle for NVMe-TCP hosts' \
'dhchap:DH-HMAC-CHAP in-band auth lifecycle for NVMe-TCP hosts' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa nvmetcp help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap_commands() {
    local commands; commands=(
'list:List every registered host DH-HMAC-CHAP entry' \
'add:Add a new host DH-HMAC-CHAP secret' \
'grant:Grant a host access to one or more volumes' \
'revoke:Revoke a host'\''s access to one or more volumes' \
'remove:Remove a host DH-HMAC-CHAP entry' \
'disable:Disable a host entry without removing it' \
'enable:Re-enable a previously disabled host entry' \
'set-ctrl-key:Set (or replace) a host'\''s controller secret for mutual auth' \
'clear-ctrl-key:Clear a host'\''s controller secret (disable mutual auth)' \
'rotate:Rotate a host'\''s DH-HMAC-CHAP secret with a grace window' \
    )
    _describe -t commands 'thurvsa nvmetcp help dhchap commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__add_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help dhchap add commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__clear-ctrl-key_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__clear-ctrl-key_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help dhchap clear-ctrl-key commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__disable_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help dhchap disable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__enable_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help dhchap enable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__grant_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__grant_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help dhchap grant commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__list_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help dhchap list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__remove_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help dhchap remove commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__revoke_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__revoke_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help dhchap revoke commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help dhchap rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__set-ctrl-key_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__dhchap__subcmd__set-ctrl-key_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help dhchap set-ctrl-key commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks_commands() {
    local commands; commands=(
'list:List every registered host PSK' \
'add:Add a new host PSK' \
'grant:Grant a host access to one or more volumes' \
'revoke:Revoke a host'\''s access to one or more volumes' \
'remove:Remove a host PSK' \
'disable:Disable a host PSK without removing the entry' \
'enable:Re-enable a previously disabled host PSK' \
'rotate:Rotate a host'\''s PSK with a grace window' \
    )
    _describe -t commands 'thurvsa nvmetcp help psks commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__add_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help psks add commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__disable_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help psks disable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__enable_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help psks enable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__grant_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__grant_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help psks grant commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__list_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help psks list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__remove_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help psks remove commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__revoke_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__revoke_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help psks revoke commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp help psks rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks_commands() {
    local commands; commands=(
'list:List every registered host PSK' \
'add:Add a new host PSK' \
'grant:Grant a host access to one or more volumes' \
'revoke:Revoke a host'\''s access to one or more volumes' \
'remove:Remove a host PSK' \
'disable:Disable a host PSK without removing the entry' \
'enable:Re-enable a previously disabled host PSK' \
'rotate:Rotate a host'\''s PSK with a grace window' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa nvmetcp psks commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__add_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks add commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__disable_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks disable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__enable_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks enable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__grant_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__grant_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks grant commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help_commands() {
    local commands; commands=(
'list:List every registered host PSK' \
'add:Add a new host PSK' \
'grant:Grant a host access to one or more volumes' \
'revoke:Revoke a host'\''s access to one or more volumes' \
'remove:Remove a host PSK' \
'disable:Disable a host PSK without removing the entry' \
'enable:Re-enable a previously disabled host PSK' \
'rotate:Rotate a host'\''s PSK with a grace window' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa nvmetcp psks help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__add_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks help add commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__disable_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks help disable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__enable_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks help enable commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__grant_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__grant_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks help grant commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__list_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks help list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__remove_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks help remove commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__revoke_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__revoke_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks help revoke commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks help rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__list_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__remove_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks remove commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__revoke_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__revoke_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks revoke commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa nvmetcp psks rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system_commands] )) ||
_thurvsa__subcmd__system_commands() {
    local commands; commands=(
'storage:Storage-backend operations' \
'gc:Garbage-collect orphan chunks from the chunk pool' \
'regenerate-cert:Regenerate the admin HTTP self-signed TLS cert' \
'alerting:First-party alerting (email + webhook)' \
'daemon-health:Probe the daemon'\''s admin Unix socket' \
'monitor:Live activity screen — holds and redraws ~1s, Ctrl-C to exit' \
'audit:Audit-chain operations' \
'stats:Dedup ratio and per-volume contribution' \
'verify:Volume-wide consistency check' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa system commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__alerting_commands] )) ||
_thurvsa__subcmd__system__subcmd__alerting_commands() {
    local commands; commands=(
'list:Show configured alert sinks and dedup window' \
'test:Fire a synthetic alert through one sink' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa system alerting commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__alerting__subcmd__help_commands] )) ||
_thurvsa__subcmd__system__subcmd__alerting__subcmd__help_commands() {
    local commands; commands=(
'list:Show configured alert sinks and dedup window' \
'test:Fire a synthetic alert through one sink' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa system alerting help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__alerting__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__system__subcmd__alerting__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system alerting help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__alerting__subcmd__help__subcmd__list_commands] )) ||
_thurvsa__subcmd__system__subcmd__alerting__subcmd__help__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system alerting help list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__alerting__subcmd__help__subcmd__test_commands] )) ||
_thurvsa__subcmd__system__subcmd__alerting__subcmd__help__subcmd__test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system alerting help test commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__alerting__subcmd__list_commands] )) ||
_thurvsa__subcmd__system__subcmd__alerting__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system alerting list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__alerting__subcmd__test_commands] )) ||
_thurvsa__subcmd__system__subcmd__alerting__subcmd__test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system alerting test commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__audit_commands] )) ||
_thurvsa__subcmd__system__subcmd__audit_commands() {
    local commands; commands=(
'tail:Print recent audit entries (optionally follow with -f)' \
'export:Export entries in the requested date range' \
'verify:Verify the tamper-evident chain end-to-end' \
'verify-offline:Offline-verify a copy of an audit directory' \
'rotate:Operator-acknowledged chain reset after a verify failure' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa system audit commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__audit__subcmd__export_commands] )) ||
_thurvsa__subcmd__system__subcmd__audit__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system audit export commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__audit__subcmd__help_commands] )) ||
_thurvsa__subcmd__system__subcmd__audit__subcmd__help_commands() {
    local commands; commands=(
'tail:Print recent audit entries (optionally follow with -f)' \
'export:Export entries in the requested date range' \
'verify:Verify the tamper-evident chain end-to-end' \
'verify-offline:Offline-verify a copy of an audit directory' \
'rotate:Operator-acknowledged chain reset after a verify failure' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa system audit help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__export_commands] )) ||
_thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system audit help export commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system audit help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system audit help rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__tail_commands] )) ||
_thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__tail_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system audit help tail commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify_commands] )) ||
_thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system audit help verify commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify-offline_commands] )) ||
_thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify-offline_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system audit help verify-offline commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__audit__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__system__subcmd__audit__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system audit rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__audit__subcmd__tail_commands] )) ||
_thurvsa__subcmd__system__subcmd__audit__subcmd__tail_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system audit tail commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__audit__subcmd__verify_commands] )) ||
_thurvsa__subcmd__system__subcmd__audit__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system audit verify commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__audit__subcmd__verify-offline_commands] )) ||
_thurvsa__subcmd__system__subcmd__audit__subcmd__verify-offline_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system audit verify-offline commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__daemon-health_commands] )) ||
_thurvsa__subcmd__system__subcmd__daemon-health_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system daemon-health commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__gc_commands] )) ||
_thurvsa__subcmd__system__subcmd__gc_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system gc commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help_commands] )) ||
_thurvsa__subcmd__system__subcmd__help_commands() {
    local commands; commands=(
'storage:Storage-backend operations' \
'gc:Garbage-collect orphan chunks from the chunk pool' \
'regenerate-cert:Regenerate the admin HTTP self-signed TLS cert' \
'alerting:First-party alerting (email + webhook)' \
'daemon-health:Probe the daemon'\''s admin Unix socket' \
'monitor:Live activity screen — holds and redraws ~1s, Ctrl-C to exit' \
'audit:Audit-chain operations' \
'stats:Dedup ratio and per-volume contribution' \
'verify:Volume-wide consistency check' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa system help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__alerting_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__alerting_commands() {
    local commands; commands=(
'list:Show configured alert sinks and dedup window' \
'test:Fire a synthetic alert through one sink' \
    )
    _describe -t commands 'thurvsa system help alerting commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__alerting__subcmd__list_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__alerting__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help alerting list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__alerting__subcmd__test_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__alerting__subcmd__test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help alerting test commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__audit_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__audit_commands() {
    local commands; commands=(
'tail:Print recent audit entries (optionally follow with -f)' \
'export:Export entries in the requested date range' \
'verify:Verify the tamper-evident chain end-to-end' \
'verify-offline:Offline-verify a copy of an audit directory' \
'rotate:Operator-acknowledged chain reset after a verify failure' \
    )
    _describe -t commands 'thurvsa system help audit commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__export_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help audit export commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__rotate_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help audit rotate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__tail_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__tail_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help audit tail commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help audit verify commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify-offline_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify-offline_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help audit verify-offline commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__daemon-health_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__daemon-health_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help daemon-health commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__gc_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__gc_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help gc commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__monitor_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__monitor_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help monitor commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__regenerate-cert_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__regenerate-cert_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help regenerate-cert commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__stats_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help stats commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__storage_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__storage_commands() {
    local commands; commands=(
'check:Check storage-backend connectivity, auth, and read/write/delete' \
'benchmark:First-party storage-backend throughput benchmark (daemon-down)' \
    )
    _describe -t commands 'thurvsa system help storage commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__storage__subcmd__benchmark_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__storage__subcmd__benchmark_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help storage benchmark commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__storage__subcmd__check_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__storage__subcmd__check_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help storage check commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__help__subcmd__verify_commands] )) ||
_thurvsa__subcmd__system__subcmd__help__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system help verify commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__monitor_commands] )) ||
_thurvsa__subcmd__system__subcmd__monitor_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system monitor commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__regenerate-cert_commands] )) ||
_thurvsa__subcmd__system__subcmd__regenerate-cert_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system regenerate-cert commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__stats_commands] )) ||
_thurvsa__subcmd__system__subcmd__stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system stats commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__storage_commands] )) ||
_thurvsa__subcmd__system__subcmd__storage_commands() {
    local commands; commands=(
'check:Check storage-backend connectivity, auth, and read/write/delete' \
'benchmark:First-party storage-backend throughput benchmark (daemon-down)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa system storage commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__storage__subcmd__benchmark_commands] )) ||
_thurvsa__subcmd__system__subcmd__storage__subcmd__benchmark_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system storage benchmark commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__storage__subcmd__check_commands] )) ||
_thurvsa__subcmd__system__subcmd__storage__subcmd__check_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system storage check commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__storage__subcmd__help_commands] )) ||
_thurvsa__subcmd__system__subcmd__storage__subcmd__help_commands() {
    local commands; commands=(
'check:Check storage-backend connectivity, auth, and read/write/delete' \
'benchmark:First-party storage-backend throughput benchmark (daemon-down)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa system storage help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__storage__subcmd__help__subcmd__benchmark_commands] )) ||
_thurvsa__subcmd__system__subcmd__storage__subcmd__help__subcmd__benchmark_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system storage help benchmark commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__storage__subcmd__help__subcmd__check_commands] )) ||
_thurvsa__subcmd__system__subcmd__storage__subcmd__help__subcmd__check_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system storage help check commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__storage__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__system__subcmd__storage__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system storage help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__system__subcmd__verify_commands] )) ||
_thurvsa__subcmd__system__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa system verify commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume_commands] )) ||
_thurvsa__subcmd__volume_commands() {
    local commands; commands=(
'create:Create a new volume' \
'list:List every volume' \
'info:Show one volume'\''s manifest' \
'destroy:Destroy a volume' \
'modify:Modify a live volume'\''s mutable settings' \
'resize:Grow a volume'\''s capacity' \
'key:Per-volume key-management operations' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa volume commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__create_commands] )) ||
_thurvsa__subcmd__volume__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume create commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__destroy_commands] )) ||
_thurvsa__subcmd__volume__subcmd__destroy_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume destroy commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__help_commands] )) ||
_thurvsa__subcmd__volume__subcmd__help_commands() {
    local commands; commands=(
'create:Create a new volume' \
'list:List every volume' \
'info:Show one volume'\''s manifest' \
'destroy:Destroy a volume' \
'modify:Modify a live volume'\''s mutable settings' \
'resize:Grow a volume'\''s capacity' \
'key:Per-volume key-management operations' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa volume help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__help__subcmd__create_commands] )) ||
_thurvsa__subcmd__volume__subcmd__help__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume help create commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__help__subcmd__destroy_commands] )) ||
_thurvsa__subcmd__volume__subcmd__help__subcmd__destroy_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume help destroy commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__volume__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__help__subcmd__info_commands] )) ||
_thurvsa__subcmd__volume__subcmd__help__subcmd__info_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume help info commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__help__subcmd__key_commands] )) ||
_thurvsa__subcmd__volume__subcmd__help__subcmd__key_commands() {
    local commands; commands=(
'migrate:Move a volume'\''s DEK to a different keystore backend' \
'export:Export a volume'\''s DEK to a passphrase-sealed file (JWE/PBES2)' \
'import:Import a passphrase-sealed DEK envelope into a volume' \
    )
    _describe -t commands 'thurvsa volume help key commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__help__subcmd__key__subcmd__export_commands] )) ||
_thurvsa__subcmd__volume__subcmd__help__subcmd__key__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume help key export commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__help__subcmd__key__subcmd__import_commands] )) ||
_thurvsa__subcmd__volume__subcmd__help__subcmd__key__subcmd__import_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume help key import commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__help__subcmd__key__subcmd__migrate_commands] )) ||
_thurvsa__subcmd__volume__subcmd__help__subcmd__key__subcmd__migrate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume help key migrate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__help__subcmd__list_commands] )) ||
_thurvsa__subcmd__volume__subcmd__help__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume help list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__help__subcmd__modify_commands] )) ||
_thurvsa__subcmd__volume__subcmd__help__subcmd__modify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume help modify commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__help__subcmd__resize_commands] )) ||
_thurvsa__subcmd__volume__subcmd__help__subcmd__resize_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume help resize commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__info_commands] )) ||
_thurvsa__subcmd__volume__subcmd__info_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume info commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__key_commands] )) ||
_thurvsa__subcmd__volume__subcmd__key_commands() {
    local commands; commands=(
'migrate:Move a volume'\''s DEK to a different keystore backend' \
'export:Export a volume'\''s DEK to a passphrase-sealed file (JWE/PBES2)' \
'import:Import a passphrase-sealed DEK envelope into a volume' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa volume key commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__key__subcmd__export_commands] )) ||
_thurvsa__subcmd__volume__subcmd__key__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume key export commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__key__subcmd__help_commands] )) ||
_thurvsa__subcmd__volume__subcmd__key__subcmd__help_commands() {
    local commands; commands=(
'migrate:Move a volume'\''s DEK to a different keystore backend' \
'export:Export a volume'\''s DEK to a passphrase-sealed file (JWE/PBES2)' \
'import:Import a passphrase-sealed DEK envelope into a volume' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvsa volume key help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__export_commands] )) ||
_thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume key help export commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__help_commands] )) ||
_thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume key help help commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__import_commands] )) ||
_thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__import_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume key help import commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__migrate_commands] )) ||
_thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__migrate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume key help migrate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__key__subcmd__import_commands] )) ||
_thurvsa__subcmd__volume__subcmd__key__subcmd__import_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume key import commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__key__subcmd__migrate_commands] )) ||
_thurvsa__subcmd__volume__subcmd__key__subcmd__migrate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume key migrate commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__list_commands] )) ||
_thurvsa__subcmd__volume__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume list commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__modify_commands] )) ||
_thurvsa__subcmd__volume__subcmd__modify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume modify commands' commands "$@"
}
(( $+functions[_thurvsa__subcmd__volume__subcmd__resize_commands] )) ||
_thurvsa__subcmd__volume__subcmd__resize_commands() {
    local commands; commands=()
    _describe -t commands 'thurvsa volume resize commands' commands "$@"
}

if [ "$funcstack[1]" = "_thurvsa" ]; then
    _thurvsa "$@"
else
    compdef _thurvsa thurvsa
fi
