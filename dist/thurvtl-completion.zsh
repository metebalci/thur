#compdef thurvtl

autoload -U is-at-least

_thurvtl() {
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
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
'-V[Print version]' \
'--version[Print version]' \
":: :_thurvtl_commands" \
"*::: :->thurvtl" \
&& ret=0
    case $state in
    (thurvtl)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-command-$line[1]:"
        case $line[1] in
            (library)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__library_commands" \
"*::: :->library" \
&& ret=0

    case $state in
    (library)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-library-command-$line[1]:"
        case $line[1] in
            (info)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the response as JSON for automation]' \
'--with-cartridges[Also show summed per-cartridge byte counters]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(bounds)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the response as JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(restore)
_arguments "${_arguments_options[@]}" : \
'--backend=[Cloud backend name to restore from]:BACKEND:_default' \
'*--barcodes=[Restore only these barcodes (comma-separated)]:BARCODES:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--dry-run[List what would be restored without writing anything]' \
'--allow-existing[Skip cartridges whose local directory already exists]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(restore-archive)
_arguments "${_arguments_options[@]}" : \
'--backend=[Backend the archive lives on]:BACKEND:_default' \
'--barcode=[Source barcode the archive was created under]:BARCODE:_default' \
'--label=[Archive label]:LABEL:_default' \
'--as-barcode=[Local barcode for the restored cartridge. Defaults to the source barcode]:AS_BARCODE:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--allow-existing[Skip silently if the destination barcode already exists locally. Without this flag, an existing local dir is an error]' \
'--dry-run[Plan only — no downloads, no inventory mutation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(monitor)
_arguments "${_arguments_options[@]}" : \
'--interval=[Update interval in seconds]:INTERVAL:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(self-test)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the structured result as JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(partition)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__library__subcmd__partition_commands" \
"*::: :->partition" \
&& ret=0

    case $state in
    (partition)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-library-partition-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the response as JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(create)
_arguments "${_arguments_options[@]}" : \
'--storage-start=[Storage-slot range start (inclusive)]:STORAGE_START:_default' \
'--storage-end=[Storage-slot range end (exclusive)]:STORAGE_END:_default' \
'--mail-start=[Mail-slot range start (inclusive). Default 0 (no mail slots)]:MAIL_START:_default' \
'--mail-end=[Mail-slot range end (exclusive). Default 0 (no mail slots)]:MAIL_END:_default' \
'*--drives=[Drive ids assigned to this partition (comma-separated)]:DRIVES:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Partition name (1-64 chars, unique):_default' \
&& ret=0
;;
(modify)
_arguments "${_arguments_options[@]}" : \
'--storage-start=[]:STORAGE_START:_default' \
'--storage-end=[]:STORAGE_END:_default' \
'--mail-start=[]:MAIL_START:_default' \
'--mail-end=[]:MAIL_END:_default' \
'*--drives=[Replace the drive set (comma-separated). Pass \`--drives ""\` to clear it]:DRIVES:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Partition name to modify:_default' \
&& ret=0
;;
(delete)
_arguments "${_arguments_options[@]}" : \
'--merge-into=[Reassign the freed slots/drives to another partition]:MERGE_INTO:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Partition name to delete:_default' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__library__subcmd__partition__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-library-partition-help-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(create)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(modify)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(delete)
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
":: :_thurvtl__subcmd__library__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-library-help-command-$line[1]:"
        case $line[1] in
            (info)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(bounds)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(restore)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(restore-archive)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(monitor)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(self-test)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(partition)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__library__subcmd__help__subcmd__partition_commands" \
"*::: :->partition" \
&& ret=0

    case $state in
    (partition)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-library-help-partition-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(create)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(modify)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(delete)
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
(cartridge)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__cartridge_commands" \
"*::: :->cartridge" \
&& ret=0

    case $state in
    (cartridge)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-cartridge-command-$line[1]:"
        case $line[1] in
            (create)
_arguments "${_arguments_options[@]}" : \
'--lto-generation=[LTO generation (currently 8 only)]:LTO_GENERATION:_default' \
'--chunk-size-mb=[Chunk size in megabytes]:CHUNK_SIZE_MB:_default' \
'--chunking=[Chunking strategy\: \`fastcdc\` (default) or \`fixed\`]:CHUNKING:(fastcdc fixed)' \
'--chunking-min-kb=[FastCDC minimum chunk size in kilobytes (advanced)]:CHUNKING_MIN_KB:_default' \
'--chunking-max-kb=[FastCDC maximum chunk size in kilobytes (advanced)]:CHUNKING_MAX_KB:_default' \
'--multi=[Create N cartridges in one call (default 1)]:MULTI:_default' \
'--backend=[Cloud backend name to bind this cartridge to]:BACKEND:_default' \
'--dedup=[Dedup scope\: \`global\` (default) or \`local\`]:DEDUP:(local global)' \
'--keystore=[Keystore backend that wraps this cartridge'\''s DEK]:KEYSTORE:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--worm[Make this cartridge WORM (Write Once Read Many)]' \
'--encrypt[Enable at-rest encryption (requires --keystore)]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':barcode -- Cartridge barcode/label:_default' \
&& ret=0
;;
(archive)
_arguments "${_arguments_options[@]}" : \
'--target-backend=[Target backend name]:TARGET_BACKEND:_default' \
'--label=[1-64-char alphanumeric label (\`-\`/\`_\` allowed). Defaults to an ISO-8601 UTC timestamp]:LABEL:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--dry-run[Plan only — no PUTs]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':barcode -- Cartridge barcode:_default' \
&& ret=0
;;
(migrate)
_arguments "${_arguments_options[@]}" : \
'--target-backend=[Target backend name (must exist under \`cloud.backends\:\`)]:TARGET_BACKEND:_default' \
'--mode=[Migration mode]:MODE:(move rebind)' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--no-verify[Skip the per-chunk HEAD verify pass (rebind mode only)]' \
'--dry-run[Plan only — no mutation on source, target, or local pool]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':barcode -- Cartridge barcode:_default' \
&& ret=0
;;
(import)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':path -- Path to cartridge directory:_default' \
':slot -- Slot ID to place cartridge:_default' \
&& ret=0
;;
(export)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':slot -- Slot ID of cartridge to export:_default' \
':path -- Destination directory path:_default' \
&& ret=0
;;
(list)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the response as JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(info)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the response as JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':identifier -- Cartridge barcode or slot ID:_default' \
&& ret=0
;;
(reset-stats)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':barcode -- Cartridge barcode:_default' \
&& ret=0
;;
(legal-hold)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__cartridge__subcmd__legal-hold_commands" \
"*::: :->legal-hold" \
&& ret=0

    case $state in
    (legal-hold)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-cartridge-legal-hold-command-$line[1]:"
        case $line[1] in
            (set)
_arguments "${_arguments_options[@]}" : \
'--id=[Operator-supplied label (audit log only)]:ID:_default' \
'--reason=[Reason for the hold; recorded in the audit log only]:REASON:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':barcode -- Cartridge barcode:_default' \
&& ret=0
;;
(clear)
_arguments "${_arguments_options[@]}" : \
'--id=[Operator-supplied label of the hold being released]:ID:_default' \
'--reason=[Reason for the release; recorded in the audit log only]:REASON:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':barcode -- Cartridge barcode:_default' \
&& ret=0
;;
(status)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--full[Sweep every chunk + manifest backup, not just the sentinel]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':barcode -- Cartridge barcode:_default' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-cartridge-legal-hold-help-command-$line[1]:"
        case $line[1] in
            (set)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(clear)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(status)
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
(key)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__cartridge__subcmd__key_commands" \
"*::: :->key" \
&& ret=0

    case $state in
    (key)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-cartridge-key-command-$line[1]:"
        case $line[1] in
            (migrate)
_arguments "${_arguments_options[@]}" : \
'--to=[New keystore-backend name (must exist under \`keystore.backends\:\`)]:TO:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--purge-local[Delete the \`local\` sidecar after a successful migrate off \`local\`. Default off so a crash mid-migrate leaves the sidecar present (recoverable rollback)]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':barcode -- Cartridge barcode:_default' \
&& ret=0
;;
(show)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':barcode -- Cartridge barcode:_default' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__cartridge__subcmd__key__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-cartridge-key-help-command-$line[1]:"
        case $line[1] in
            (migrate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(show)
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
":: :_thurvtl__subcmd__cartridge__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-cartridge-help-command-$line[1]:"
        case $line[1] in
            (create)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(archive)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(migrate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(import)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(export)
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
(reset-stats)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(legal-hold)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal-hold_commands" \
"*::: :->legal-hold" \
&& ret=0

    case $state in
    (legal-hold)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-cartridge-help-legal-hold-command-$line[1]:"
        case $line[1] in
            (set)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(clear)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(status)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(key)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__cartridge__subcmd__help__subcmd__key_commands" \
"*::: :->key" \
&& ret=0

    case $state in
    (key)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-cartridge-help-key-command-$line[1]:"
        case $line[1] in
            (migrate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(show)
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
(changer)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__changer_commands" \
"*::: :->changer" \
&& ret=0

    case $state in
    (changer)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-changer-command-$line[1]:"
        case $line[1] in
            (inventory)
_arguments "${_arguments_options[@]}" : \
'--filter=[Filter by barcode pattern]:FILTER:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the response as JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(move)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--cross-partition[Allow source and destination to belong to different logical partitions. Default refuses cross-partition moves when partitions are defined; this flag is the operator- console override and is recorded in the audit log]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':from_slot -- Source slot ID:_default' \
':to_slot -- Destination slot ID:_default' \
&& ret=0
;;
(load)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--cross-partition[Allow loading from a slot in one partition into a drive in another. See \`changer move --cross-partition\` for the semantics; same audit-tag treatment]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':slot -- Source slot ID (storage or mail slot):_default' \
':drive -- Destination drive ID (0-based):_default' \
&& ret=0
;;
(unload)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--force[Bypass host-asserted PREVENT MEDIUM REMOVAL bit 1]' \
'--cross-partition[Allow unloading into a storage slot in a different partition than the drive'\''s. See \`changer move --cross-partition\` for the semantics]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':drive -- Source drive ID (0-based):_default' \
'::slot -- Destination slot ID (optional, auto-select if not specified):_default' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__changer__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-changer-help-command-$line[1]:"
        case $line[1] in
            (inventory)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(move)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(load)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(unload)
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
(drive)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__drive_commands" \
"*::: :->drive" \
&& ret=0

    case $state in
    (drive)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-drive-command-$line[1]:"
        case $line[1] in
            (status)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the response as JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':drive -- Drive ID (0-based):_default' \
&& ret=0
;;
(self-test)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the structured result as JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':drive -- Drive ID (0-based):_default' \
&& ret=0
;;
(reset-stats)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--all[Reset every drive]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
'::drive -- Drive ID (0-based). Omit when using --all:_default' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__drive__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-drive-help-command-$line[1]:"
        case $line[1] in
            (status)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(self-test)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(reset-stats)
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
(system)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__system_commands" \
"*::: :->system" \
&& ret=0

    case $state in
    (system)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-command-$line[1]:"
        case $line[1] in
            (gc)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--dry-run[Show what would be deleted without actually deleting]' \
'--storage[Also delete orphan objects from the storage backend]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(audit)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__system__subcmd__audit_commands" \
"*::: :->audit" \
&& ret=0

    case $state in
    (audit)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-audit-command-$line[1]:"
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
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(verify)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
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
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__system__subcmd__audit__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-audit-help-command-$line[1]:"
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
(storage)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__system__subcmd__storage_commands" \
"*::: :->storage" \
&& ret=0

    case $state in
    (storage)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-storage-command-$line[1]:"
        case $line[1] in
            (check)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
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
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__system__subcmd__storage__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-storage-help-command-$line[1]:"
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
(stats)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the full report as JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(daemon-health)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the response as JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(monitor)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(reset-stats)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
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
'--verbose[Per-cartridge breakdown (partitions, every error/warning)]' \
'--json[Emit the full report as JSON for CI / automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
'*::barcodes -- Optional barcodes to limit the cartridge sweep:_default' \
&& ret=0
;;
(tiering)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__system__subcmd__tiering_commands" \
"*::: :->tiering" \
&& ret=0

    case $state in
    (tiering)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-tiering-command-$line[1]:"
        case $line[1] in
            (plan)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the full plan as JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(run-now)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the full result as JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(status)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit the summary as JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__system__subcmd__tiering__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-tiering-help-command-$line[1]:"
        case $line[1] in
            (plan)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(run-now)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(status)
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
(regenerate-cert)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(set-admin-password)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(alerting)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__system__subcmd__alerting_commands" \
"*::: :->alerting" \
&& ret=0

    case $state in
    (alerting)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-alerting-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[]' \
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
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':sink -- Sink name from the YAML `alerting.sinks` list:_default' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__system__subcmd__alerting__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-alerting-help-command-$line[1]:"
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
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__system__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-help-command-$line[1]:"
        case $line[1] in
            (gc)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(audit)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__system__subcmd__help__subcmd__audit_commands" \
"*::: :->audit" \
&& ret=0

    case $state in
    (audit)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-help-audit-command-$line[1]:"
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
(storage)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__system__subcmd__help__subcmd__storage_commands" \
"*::: :->storage" \
&& ret=0

    case $state in
    (storage)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-help-storage-command-$line[1]:"
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
(stats)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(daemon-health)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(monitor)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(reset-stats)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(verify)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(tiering)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__system__subcmd__help__subcmd__tiering_commands" \
"*::: :->tiering" \
&& ret=0

    case $state in
    (tiering)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-help-tiering-command-$line[1]:"
        case $line[1] in
            (plan)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(run-now)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(status)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(regenerate-cert)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(set-admin-password)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(alerting)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__system__subcmd__help__subcmd__alerting_commands" \
"*::: :->alerting" \
&& ret=0

    case $state in
    (alerting)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-system-help-alerting-command-$line[1]:"
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
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__iscsi_commands" \
"*::: :->iscsi" \
&& ret=0

    case $state in
    (iscsi)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-iscsi-command-$line[1]:"
        case $line[1] in
            (users)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__iscsi__subcmd__users_commands" \
"*::: :->users" \
&& ret=0

    case $state in
    (users)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-iscsi-users-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(add)
_arguments "${_arguments_options[@]}" : \
'(--password-stdin)--password=[Password as a CLI arg. Mutually exclusive with \`--password-stdin\`]:PASSWORD:_default' \
'--partition=[Partition the user is fenced to]:PARTITION:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--password-stdin[Read the password from stdin (single line)]' \
'--mutual-chap[Enable mutual CHAP (target authenticates back)]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Username (CHAP identity the initiator presents):_default' \
&& ret=0
;;
(remove)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name:_default' \
&& ret=0
;;
(disable)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name:_default' \
&& ret=0
;;
(enable)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name:_default' \
&& ret=0
;;
(rotate)
_arguments "${_arguments_options[@]}" : \
'(--password-stdin --cancel)--password=[New password as a CLI arg. Mutually exclusive with \`--password-stdin\`]:PASSWORD:_default' \
'(--cancel)--grace=[Grace window (humantime\: \`24h\`, \`5m\`, \`1d12h\`). Default \`24h\`]:GRACE:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'(--cancel)--password-stdin[Read the new password from stdin (single line)]' \
'--cancel[Cancel an in-flight rotation\: drop the new password, restore the previous one]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
':name -- Username to rotate:_default' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-iscsi-users-help-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(add)
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
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__iscsi__subcmd__target_commands" \
"*::: :->target" \
&& ret=0

    case $state in
    (target)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-iscsi-target-command-$line[1]:"
        case $line[1] in
            (show)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--json[Emit JSON for automation]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(set)
_arguments "${_arguments_options[@]}" : \
'--username=[Target username]:USERNAME:_default' \
'(--password-stdin)--password=[Password as a CLI arg. Mutually exclusive with \`--password-stdin\`]:PASSWORD:_default' \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'--password-stdin[Read the password from stdin (single line)]' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(clear)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__iscsi__subcmd__target__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-iscsi-target-help-command-$line[1]:"
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
":: :_thurvtl__subcmd__iscsi__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-iscsi-help-command-$line[1]:"
        case $line[1] in
            (users)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users_commands" \
"*::: :->users" \
&& ret=0

    case $state in
    (users)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-iscsi-help-users-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(add)
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
":: :_thurvtl__subcmd__iscsi__subcmd__help__subcmd__target_commands" \
"*::: :->target" \
&& ret=0

    case $state in
    (target)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-iscsi-help-target-command-$line[1]:"
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
(config)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
":: :_thurvtl__subcmd__config_commands" \
"*::: :->config" \
&& ret=0

    case $state in
    (config)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-config-command-$line[1]:"
        case $line[1] in
            (defaults)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(systemd-unit)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
&& ret=0
;;
(completion)
_arguments "${_arguments_options[@]}" : \
'-c+[Path to configuration file]:CONFIG:_default' \
'--config=[Path to configuration file]:CONFIG:_default' \
'--user=[User to drop privileges to under sudo]:USER:_default' \
'-h[Print help (see more with '\''--help'\'')]' \
'--help[Print help (see more with '\''--help'\'')]' \
'::shell -- Target shell. Defaults to `basename $SHELL`:(bash elvish fish powershell zsh)' \
&& ret=0
;;
(help)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__config__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-config-help-command-$line[1]:"
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
":: :_thurvtl__subcmd__help_commands" \
"*::: :->help" \
&& ret=0

    case $state in
    (help)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-command-$line[1]:"
        case $line[1] in
            (library)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__library_commands" \
"*::: :->library" \
&& ret=0

    case $state in
    (library)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-library-command-$line[1]:"
        case $line[1] in
            (info)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(bounds)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(restore)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(restore-archive)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(monitor)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(self-test)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(partition)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__library__subcmd__partition_commands" \
"*::: :->partition" \
&& ret=0

    case $state in
    (partition)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-library-partition-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(create)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(modify)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(delete)
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
(cartridge)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__cartridge_commands" \
"*::: :->cartridge" \
&& ret=0

    case $state in
    (cartridge)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-cartridge-command-$line[1]:"
        case $line[1] in
            (create)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(archive)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(migrate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(import)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(export)
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
(reset-stats)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(legal-hold)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal-hold_commands" \
"*::: :->legal-hold" \
&& ret=0

    case $state in
    (legal-hold)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-cartridge-legal-hold-command-$line[1]:"
        case $line[1] in
            (set)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(clear)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(status)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(key)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__cartridge__subcmd__key_commands" \
"*::: :->key" \
&& ret=0

    case $state in
    (key)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-cartridge-key-command-$line[1]:"
        case $line[1] in
            (migrate)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(show)
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
(changer)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__changer_commands" \
"*::: :->changer" \
&& ret=0

    case $state in
    (changer)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-changer-command-$line[1]:"
        case $line[1] in
            (inventory)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(move)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(load)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(unload)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(drive)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__drive_commands" \
"*::: :->drive" \
&& ret=0

    case $state in
    (drive)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-drive-command-$line[1]:"
        case $line[1] in
            (status)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(self-test)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(reset-stats)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(system)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__system_commands" \
"*::: :->system" \
&& ret=0

    case $state in
    (system)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-system-command-$line[1]:"
        case $line[1] in
            (gc)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(audit)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__system__subcmd__audit_commands" \
"*::: :->audit" \
&& ret=0

    case $state in
    (audit)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-system-audit-command-$line[1]:"
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
(storage)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__system__subcmd__storage_commands" \
"*::: :->storage" \
&& ret=0

    case $state in
    (storage)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-system-storage-command-$line[1]:"
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
(stats)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(daemon-health)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(monitor)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(reset-stats)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(verify)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(tiering)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__system__subcmd__tiering_commands" \
"*::: :->tiering" \
&& ret=0

    case $state in
    (tiering)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-system-tiering-command-$line[1]:"
        case $line[1] in
            (plan)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(run-now)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(status)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
        esac
    ;;
esac
;;
(regenerate-cert)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(set-admin-password)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(alerting)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__system__subcmd__alerting_commands" \
"*::: :->alerting" \
&& ret=0

    case $state in
    (alerting)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-system-alerting-command-$line[1]:"
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
        esac
    ;;
esac
;;
(iscsi)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__iscsi_commands" \
"*::: :->iscsi" \
&& ret=0

    case $state in
    (iscsi)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-iscsi-command-$line[1]:"
        case $line[1] in
            (users)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users_commands" \
"*::: :->users" \
&& ret=0

    case $state in
    (users)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-iscsi-users-command-$line[1]:"
        case $line[1] in
            (list)
_arguments "${_arguments_options[@]}" : \
&& ret=0
;;
(add)
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
":: :_thurvtl__subcmd__help__subcmd__iscsi__subcmd__target_commands" \
"*::: :->target" \
&& ret=0

    case $state in
    (target)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-iscsi-target-command-$line[1]:"
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
(config)
_arguments "${_arguments_options[@]}" : \
":: :_thurvtl__subcmd__help__subcmd__config_commands" \
"*::: :->config" \
&& ret=0

    case $state in
    (config)
        words=($line[1] "${words[@]}")
        (( CURRENT += 1 ))
        curcontext="${curcontext%:*:*}:thurvtl-help-config-command-$line[1]:"
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

(( $+functions[_thurvtl_commands] )) ||
_thurvtl_commands() {
    local commands; commands=(
'library:Library management (init, info, modify, monitor)' \
'cartridge:Cartridge management operations' \
'changer:Changer / SMC operations (inventory, move, load, unload)' \
'drive:Drive operations and status' \
'system:System operations' \
'iscsi:iSCSI CHAP credentials' \
'config:Configuration helpers (defaults yaml, systemd unit, shell completion)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge_commands] )) ||
_thurvtl__subcmd__cartridge_commands() {
    local commands; commands=(
'create:Create new blank cartridge (places in first available slot)' \
'archive:Archive a cartridge to a different cloud backend' \
'migrate:Move a cartridge to a different cloud backend' \
'import:Import existing cartridge from filesystem' \
'export:Export cartridge to filesystem' \
'list:List all cartridges with metadata' \
'info:Show detailed cartridge information' \
'reset-stats:Reset a cartridge'\''s activity stats to zero' \
'legal-hold:Per-cartridge legal hold (cloud-native)' \
'key:At-rest encryption DEK management' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl cartridge commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__archive_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__archive_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge archive commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__create_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge create commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__export_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge export commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help_commands() {
    local commands; commands=(
'create:Create new blank cartridge (places in first available slot)' \
'archive:Archive a cartridge to a different cloud backend' \
'migrate:Move a cartridge to a different cloud backend' \
'import:Import existing cartridge from filesystem' \
'export:Export cartridge to filesystem' \
'list:List all cartridges with metadata' \
'info:Show detailed cartridge information' \
'reset-stats:Reset a cartridge'\''s activity stats to zero' \
'legal-hold:Per-cartridge legal hold (cloud-native)' \
'key:At-rest encryption DEK management' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl cartridge help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__archive_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__archive_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help archive commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__create_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help create commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__export_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help export commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__import_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__import_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help import commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__info_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__info_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help info commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__key_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__key_commands() {
    local commands; commands=(
'migrate:Move a cartridge'\''s DEK wrap-target to a different keystore' \
'show:Show a cartridge'\''s at-rest encryption metadata' \
    )
    _describe -t commands 'thurvtl cartridge help key commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__key__subcmd__migrate_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__key__subcmd__migrate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help key migrate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__key__subcmd__show_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__key__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help key show commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal-hold_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal-hold_commands() {
    local commands; commands=(
'set:Engage legal hold on the cartridge'\''s cloud objects' \
'clear:Release legal hold on the cartridge'\''s cloud objects' \
'status:Read legal-hold state from the cloud provider' \
    )
    _describe -t commands 'thurvtl cartridge help legal-hold commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal-hold__subcmd__clear_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal-hold__subcmd__clear_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help legal-hold clear commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal-hold__subcmd__set_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal-hold__subcmd__set_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help legal-hold set commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal-hold__subcmd__status_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal-hold__subcmd__status_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help legal-hold status commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__list_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__migrate_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__migrate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help migrate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__help__subcmd__reset-stats_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__help__subcmd__reset-stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge help reset-stats commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__import_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__import_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge import commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__info_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__info_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge info commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__key_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__key_commands() {
    local commands; commands=(
'migrate:Move a cartridge'\''s DEK wrap-target to a different keystore' \
'show:Show a cartridge'\''s at-rest encryption metadata' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl cartridge key commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__key__subcmd__help_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__key__subcmd__help_commands() {
    local commands; commands=(
'migrate:Move a cartridge'\''s DEK wrap-target to a different keystore' \
'show:Show a cartridge'\''s at-rest encryption metadata' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl cartridge key help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__key__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__key__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge key help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__key__subcmd__help__subcmd__migrate_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__key__subcmd__help__subcmd__migrate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge key help migrate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__key__subcmd__help__subcmd__show_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__key__subcmd__help__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge key help show commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__key__subcmd__migrate_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__key__subcmd__migrate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge key migrate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__key__subcmd__show_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__key__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge key show commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__legal-hold_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__legal-hold_commands() {
    local commands; commands=(
'set:Engage legal hold on the cartridge'\''s cloud objects' \
'clear:Release legal hold on the cartridge'\''s cloud objects' \
'status:Read legal-hold state from the cloud provider' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl cartridge legal-hold commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__clear_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__clear_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge legal-hold clear commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__help_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__help_commands() {
    local commands; commands=(
'set:Engage legal hold on the cartridge'\''s cloud objects' \
'clear:Release legal hold on the cartridge'\''s cloud objects' \
'status:Read legal-hold state from the cloud provider' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl cartridge legal-hold help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__help__subcmd__clear_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__help__subcmd__clear_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge legal-hold help clear commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge legal-hold help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__help__subcmd__set_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__help__subcmd__set_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge legal-hold help set commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__help__subcmd__status_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__help__subcmd__status_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge legal-hold help status commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__set_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__set_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge legal-hold set commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__status_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__legal-hold__subcmd__status_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge legal-hold status commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__list_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__migrate_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__migrate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge migrate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__cartridge__subcmd__reset-stats_commands] )) ||
_thurvtl__subcmd__cartridge__subcmd__reset-stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl cartridge reset-stats commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__changer_commands] )) ||
_thurvtl__subcmd__changer_commands() {
    local commands; commands=(
'inventory:List all cartridges in the library' \
'move:Move cartridge from one slot to another (changes home slot)' \
'load:Load cartridge from slot to drive' \
'unload:Unload cartridge from drive to slot' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl changer commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__changer__subcmd__help_commands] )) ||
_thurvtl__subcmd__changer__subcmd__help_commands() {
    local commands; commands=(
'inventory:List all cartridges in the library' \
'move:Move cartridge from one slot to another (changes home slot)' \
'load:Load cartridge from slot to drive' \
'unload:Unload cartridge from drive to slot' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl changer help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__changer__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__changer__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl changer help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__changer__subcmd__help__subcmd__inventory_commands] )) ||
_thurvtl__subcmd__changer__subcmd__help__subcmd__inventory_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl changer help inventory commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__changer__subcmd__help__subcmd__load_commands] )) ||
_thurvtl__subcmd__changer__subcmd__help__subcmd__load_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl changer help load commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__changer__subcmd__help__subcmd__move_commands] )) ||
_thurvtl__subcmd__changer__subcmd__help__subcmd__move_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl changer help move commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__changer__subcmd__help__subcmd__unload_commands] )) ||
_thurvtl__subcmd__changer__subcmd__help__subcmd__unload_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl changer help unload commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__changer__subcmd__inventory_commands] )) ||
_thurvtl__subcmd__changer__subcmd__inventory_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl changer inventory commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__changer__subcmd__load_commands] )) ||
_thurvtl__subcmd__changer__subcmd__load_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl changer load commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__changer__subcmd__move_commands] )) ||
_thurvtl__subcmd__changer__subcmd__move_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl changer move commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__changer__subcmd__unload_commands] )) ||
_thurvtl__subcmd__changer__subcmd__unload_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl changer unload commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__config_commands] )) ||
_thurvtl__subcmd__config_commands() {
    local commands; commands=(
'defaults:Emit the default configuration yaml on stdout' \
'systemd-unit:Emit the default systemd unit file on stdout' \
'completion:Emit a shell completion script on stdout' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl config commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__config__subcmd__completion_commands] )) ||
_thurvtl__subcmd__config__subcmd__completion_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl config completion commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__config__subcmd__defaults_commands] )) ||
_thurvtl__subcmd__config__subcmd__defaults_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl config defaults commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__config__subcmd__help_commands] )) ||
_thurvtl__subcmd__config__subcmd__help_commands() {
    local commands; commands=(
'defaults:Emit the default configuration yaml on stdout' \
'systemd-unit:Emit the default systemd unit file on stdout' \
'completion:Emit a shell completion script on stdout' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl config help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__config__subcmd__help__subcmd__completion_commands] )) ||
_thurvtl__subcmd__config__subcmd__help__subcmd__completion_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl config help completion commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__config__subcmd__help__subcmd__defaults_commands] )) ||
_thurvtl__subcmd__config__subcmd__help__subcmd__defaults_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl config help defaults commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__config__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__config__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl config help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__config__subcmd__help__subcmd__systemd-unit_commands] )) ||
_thurvtl__subcmd__config__subcmd__help__subcmd__systemd-unit_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl config help systemd-unit commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__config__subcmd__systemd-unit_commands] )) ||
_thurvtl__subcmd__config__subcmd__systemd-unit_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl config systemd-unit commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__drive_commands] )) ||
_thurvtl__subcmd__drive_commands() {
    local commands; commands=(
'status:Show drive status and current operation' \
'self-test:Run the SPC-4 self-test against a drive LUN' \
'reset-stats:Reset a drive'\''s lifetime stats to zero' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl drive commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__drive__subcmd__help_commands] )) ||
_thurvtl__subcmd__drive__subcmd__help_commands() {
    local commands; commands=(
'status:Show drive status and current operation' \
'self-test:Run the SPC-4 self-test against a drive LUN' \
'reset-stats:Reset a drive'\''s lifetime stats to zero' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl drive help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__drive__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__drive__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl drive help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__drive__subcmd__help__subcmd__reset-stats_commands] )) ||
_thurvtl__subcmd__drive__subcmd__help__subcmd__reset-stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl drive help reset-stats commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__drive__subcmd__help__subcmd__self-test_commands] )) ||
_thurvtl__subcmd__drive__subcmd__help__subcmd__self-test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl drive help self-test commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__drive__subcmd__help__subcmd__status_commands] )) ||
_thurvtl__subcmd__drive__subcmd__help__subcmd__status_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl drive help status commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__drive__subcmd__reset-stats_commands] )) ||
_thurvtl__subcmd__drive__subcmd__reset-stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl drive reset-stats commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__drive__subcmd__self-test_commands] )) ||
_thurvtl__subcmd__drive__subcmd__self-test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl drive self-test commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__drive__subcmd__status_commands] )) ||
_thurvtl__subcmd__drive__subcmd__status_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl drive status commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help_commands] )) ||
_thurvtl__subcmd__help_commands() {
    local commands; commands=(
'library:Library management (init, info, modify, monitor)' \
'cartridge:Cartridge management operations' \
'changer:Changer / SMC operations (inventory, move, load, unload)' \
'drive:Drive operations and status' \
'system:System operations' \
'iscsi:iSCSI CHAP credentials' \
'config:Configuration helpers (defaults yaml, systemd unit, shell completion)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge_commands() {
    local commands; commands=(
'create:Create new blank cartridge (places in first available slot)' \
'archive:Archive a cartridge to a different cloud backend' \
'migrate:Move a cartridge to a different cloud backend' \
'import:Import existing cartridge from filesystem' \
'export:Export cartridge to filesystem' \
'list:List all cartridges with metadata' \
'info:Show detailed cartridge information' \
'reset-stats:Reset a cartridge'\''s activity stats to zero' \
'legal-hold:Per-cartridge legal hold (cloud-native)' \
'key:At-rest encryption DEK management' \
    )
    _describe -t commands 'thurvtl help cartridge commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__archive_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__archive_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help cartridge archive commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__create_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help cartridge create commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__export_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help cartridge export commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__import_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__import_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help cartridge import commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__info_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__info_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help cartridge info commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__key_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__key_commands() {
    local commands; commands=(
'migrate:Move a cartridge'\''s DEK wrap-target to a different keystore' \
'show:Show a cartridge'\''s at-rest encryption metadata' \
    )
    _describe -t commands 'thurvtl help cartridge key commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__key__subcmd__migrate_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__key__subcmd__migrate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help cartridge key migrate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__key__subcmd__show_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__key__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help cartridge key show commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal-hold_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal-hold_commands() {
    local commands; commands=(
'set:Engage legal hold on the cartridge'\''s cloud objects' \
'clear:Release legal hold on the cartridge'\''s cloud objects' \
'status:Read legal-hold state from the cloud provider' \
    )
    _describe -t commands 'thurvtl help cartridge legal-hold commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal-hold__subcmd__clear_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal-hold__subcmd__clear_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help cartridge legal-hold clear commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal-hold__subcmd__set_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal-hold__subcmd__set_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help cartridge legal-hold set commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal-hold__subcmd__status_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal-hold__subcmd__status_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help cartridge legal-hold status commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__list_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help cartridge list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__migrate_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__migrate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help cartridge migrate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__cartridge__subcmd__reset-stats_commands] )) ||
_thurvtl__subcmd__help__subcmd__cartridge__subcmd__reset-stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help cartridge reset-stats commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__changer_commands] )) ||
_thurvtl__subcmd__help__subcmd__changer_commands() {
    local commands; commands=(
'inventory:List all cartridges in the library' \
'move:Move cartridge from one slot to another (changes home slot)' \
'load:Load cartridge from slot to drive' \
'unload:Unload cartridge from drive to slot' \
    )
    _describe -t commands 'thurvtl help changer commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__changer__subcmd__inventory_commands] )) ||
_thurvtl__subcmd__help__subcmd__changer__subcmd__inventory_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help changer inventory commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__changer__subcmd__load_commands] )) ||
_thurvtl__subcmd__help__subcmd__changer__subcmd__load_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help changer load commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__changer__subcmd__move_commands] )) ||
_thurvtl__subcmd__help__subcmd__changer__subcmd__move_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help changer move commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__changer__subcmd__unload_commands] )) ||
_thurvtl__subcmd__help__subcmd__changer__subcmd__unload_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help changer unload commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__config_commands] )) ||
_thurvtl__subcmd__help__subcmd__config_commands() {
    local commands; commands=(
'defaults:Emit the default configuration yaml on stdout' \
'systemd-unit:Emit the default systemd unit file on stdout' \
'completion:Emit a shell completion script on stdout' \
    )
    _describe -t commands 'thurvtl help config commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__config__subcmd__completion_commands] )) ||
_thurvtl__subcmd__help__subcmd__config__subcmd__completion_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help config completion commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__config__subcmd__defaults_commands] )) ||
_thurvtl__subcmd__help__subcmd__config__subcmd__defaults_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help config defaults commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__config__subcmd__systemd-unit_commands] )) ||
_thurvtl__subcmd__help__subcmd__config__subcmd__systemd-unit_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help config systemd-unit commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__drive_commands] )) ||
_thurvtl__subcmd__help__subcmd__drive_commands() {
    local commands; commands=(
'status:Show drive status and current operation' \
'self-test:Run the SPC-4 self-test against a drive LUN' \
'reset-stats:Reset a drive'\''s lifetime stats to zero' \
    )
    _describe -t commands 'thurvtl help drive commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__drive__subcmd__reset-stats_commands] )) ||
_thurvtl__subcmd__help__subcmd__drive__subcmd__reset-stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help drive reset-stats commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__drive__subcmd__self-test_commands] )) ||
_thurvtl__subcmd__help__subcmd__drive__subcmd__self-test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help drive self-test commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__drive__subcmd__status_commands] )) ||
_thurvtl__subcmd__help__subcmd__drive__subcmd__status_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help drive status commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__iscsi_commands] )) ||
_thurvtl__subcmd__help__subcmd__iscsi_commands() {
    local commands; commands=(
'users:CHAP user lifecycle' \
'target:Mutual-CHAP target credential (singleton)' \
    )
    _describe -t commands 'thurvtl help iscsi commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__iscsi__subcmd__target_commands] )) ||
_thurvtl__subcmd__help__subcmd__iscsi__subcmd__target_commands() {
    local commands; commands=(
'show:Show the current target identity (password value hidden)' \
'set:Set both target_username and target_password' \
'clear:Clear both target_username and target_password' \
    )
    _describe -t commands 'thurvtl help iscsi target commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__clear_commands] )) ||
_thurvtl__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__clear_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help iscsi target clear commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__set_commands] )) ||
_thurvtl__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__set_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help iscsi target set commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__show_commands] )) ||
_thurvtl__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help iscsi target show commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users_commands] )) ||
_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users_commands() {
    local commands; commands=(
'list:List every CHAP user' \
'add:Add a new CHAP user' \
'remove:Remove a CHAP user' \
'disable:Disable a user without removing the entry' \
'enable:Re-enable a previously disabled user' \
'rotate:Rotate a user'\''s password with a grace window' \
    )
    _describe -t commands 'thurvtl help iscsi users commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__add_commands] )) ||
_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help iscsi users add commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__disable_commands] )) ||
_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help iscsi users disable commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__enable_commands] )) ||
_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help iscsi users enable commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__list_commands] )) ||
_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help iscsi users list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__remove_commands] )) ||
_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help iscsi users remove commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__rotate_commands] )) ||
_thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help iscsi users rotate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__library_commands] )) ||
_thurvtl__subcmd__help__subcmd__library_commands() {
    local commands; commands=(
'info:Show library information' \
'bounds:Show min / current / max for num_slots and num_drives' \
'restore:Restore cartridges from a cloud backend after disaster recovery' \
'restore-archive:Pull a frozen archive back into a live cartridge' \
'monitor:Monitor library activity in real-time' \
'self-test:Run the SPC-4 self-test against the changer LUN' \
'partition:Manage logical partitions (chassis-assembly, daemon-down)' \
    )
    _describe -t commands 'thurvtl help library commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__library__subcmd__bounds_commands] )) ||
_thurvtl__subcmd__help__subcmd__library__subcmd__bounds_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help library bounds commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__library__subcmd__info_commands] )) ||
_thurvtl__subcmd__help__subcmd__library__subcmd__info_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help library info commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__library__subcmd__monitor_commands] )) ||
_thurvtl__subcmd__help__subcmd__library__subcmd__monitor_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help library monitor commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__library__subcmd__partition_commands] )) ||
_thurvtl__subcmd__help__subcmd__library__subcmd__partition_commands() {
    local commands; commands=(
'list:List defined partitions' \
'create:Create a new partition' \
'modify:Modify an existing partition' \
'delete:Delete a partition' \
    )
    _describe -t commands 'thurvtl help library partition commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__create_commands] )) ||
_thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help library partition create commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__delete_commands] )) ||
_thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__delete_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help library partition delete commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__list_commands] )) ||
_thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help library partition list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__modify_commands] )) ||
_thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__modify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help library partition modify commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__library__subcmd__restore_commands] )) ||
_thurvtl__subcmd__help__subcmd__library__subcmd__restore_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help library restore commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__library__subcmd__restore-archive_commands] )) ||
_thurvtl__subcmd__help__subcmd__library__subcmd__restore-archive_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help library restore-archive commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__library__subcmd__self-test_commands] )) ||
_thurvtl__subcmd__help__subcmd__library__subcmd__self-test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help library self-test commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system_commands] )) ||
_thurvtl__subcmd__help__subcmd__system_commands() {
    local commands; commands=(
'gc:Garbage-collect orphan chunks from the chunk pool' \
'audit:Audit-chain operations' \
'storage:Storage-backend operations' \
'stats:Dedup ratio, per-cartridge contribution, HEAD-skip rate' \
'daemon-health:Probe the daemon'\''s admin Unix socket' \
'monitor:Live activity screen — holds and redraws ~1s, Ctrl-C to exit' \
'reset-stats:Reset all activity stats to their initial state' \
'verify:Library-wide consistency check' \
'tiering:Cartridge tiering — evaluate placement policies' \
'regenerate-cert:Regenerate the admin HTTP self-signed TLS cert' \
'set-admin-password:Set the web-admin (Web UI) password' \
'alerting:First-party alerting (email + webhook)' \
    )
    _describe -t commands 'thurvtl help system commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__alerting_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__alerting_commands() {
    local commands; commands=(
'list:Show configured alert sinks and dedup window' \
'test:Fire a synthetic alert through one sink' \
    )
    _describe -t commands 'thurvtl help system alerting commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__alerting__subcmd__list_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__alerting__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system alerting list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__alerting__subcmd__test_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__alerting__subcmd__test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system alerting test commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__audit_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__audit_commands() {
    local commands; commands=(
'tail:Print recent audit entries (optionally follow with -f)' \
'export:Export entries in the requested date range' \
'verify:Verify the tamper-evident chain end-to-end' \
'verify-offline:Offline-verify a copy of an audit directory' \
'rotate:Operator-acknowledged chain reset after a verify failure' \
    )
    _describe -t commands 'thurvtl help system audit commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__export_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system audit export commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__rotate_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system audit rotate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__tail_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__tail_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system audit tail commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system audit verify commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify-offline_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify-offline_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system audit verify-offline commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__daemon-health_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__daemon-health_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system daemon-health commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__gc_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__gc_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system gc commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__monitor_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__monitor_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system monitor commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__regenerate-cert_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__regenerate-cert_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system regenerate-cert commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__reset-stats_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__reset-stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system reset-stats commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__set-admin-password_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__set-admin-password_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system set-admin-password commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__stats_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system stats commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__storage_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__storage_commands() {
    local commands; commands=(
'check:Check storage-backend connectivity, auth, and read/write/delete' \
'benchmark:First-party storage-backend throughput benchmark (daemon-down)' \
    )
    _describe -t commands 'thurvtl help system storage commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__storage__subcmd__benchmark_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__storage__subcmd__benchmark_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system storage benchmark commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__storage__subcmd__check_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__storage__subcmd__check_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system storage check commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__tiering_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__tiering_commands() {
    local commands; commands=(
'plan:Show migrations the tiering policies would trigger' \
'run-now:Apply the tiering plan now (migrates cartridges)' \
'status:Summarize tiering\: policy count and pending moves' \
    )
    _describe -t commands 'thurvtl help system tiering commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__tiering__subcmd__plan_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__tiering__subcmd__plan_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system tiering plan commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__tiering__subcmd__run-now_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__tiering__subcmd__run-now_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system tiering run-now commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__tiering__subcmd__status_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__tiering__subcmd__status_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system tiering status commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__help__subcmd__system__subcmd__verify_commands] )) ||
_thurvtl__subcmd__help__subcmd__system__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl help system verify commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi_commands] )) ||
_thurvtl__subcmd__iscsi_commands() {
    local commands; commands=(
'users:CHAP user lifecycle' \
'target:Mutual-CHAP target credential (singleton)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl iscsi commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__help_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__help_commands() {
    local commands; commands=(
'users:CHAP user lifecycle' \
'target:Mutual-CHAP target credential (singleton)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl iscsi help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__help__subcmd__target_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__help__subcmd__target_commands() {
    local commands; commands=(
'show:Show the current target identity (password value hidden)' \
'set:Set both target_username and target_password' \
'clear:Clear both target_username and target_password' \
    )
    _describe -t commands 'thurvtl iscsi help target commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__clear_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__clear_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi help target clear commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__set_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__set_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi help target set commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__show_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi help target show commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users_commands() {
    local commands; commands=(
'list:List every CHAP user' \
'add:Add a new CHAP user' \
'remove:Remove a CHAP user' \
'disable:Disable a user without removing the entry' \
'enable:Re-enable a previously disabled user' \
'rotate:Rotate a user'\''s password with a grace window' \
    )
    _describe -t commands 'thurvtl iscsi help users commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__add_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi help users add commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__disable_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi help users disable commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__enable_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi help users enable commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__list_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi help users list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__remove_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi help users remove commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__rotate_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi help users rotate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__target_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__target_commands() {
    local commands; commands=(
'show:Show the current target identity (password value hidden)' \
'set:Set both target_username and target_password' \
'clear:Clear both target_username and target_password' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl iscsi target commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__target__subcmd__clear_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__target__subcmd__clear_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi target clear commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__target__subcmd__help_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__target__subcmd__help_commands() {
    local commands; commands=(
'show:Show the current target identity (password value hidden)' \
'set:Set both target_username and target_password' \
'clear:Clear both target_username and target_password' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl iscsi target help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__clear_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__clear_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi target help clear commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi target help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__set_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__set_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi target help set commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__show_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi target help show commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__target__subcmd__set_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__target__subcmd__set_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi target set commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__target__subcmd__show_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__target__subcmd__show_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi target show commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users_commands() {
    local commands; commands=(
'list:List every CHAP user' \
'add:Add a new CHAP user' \
'remove:Remove a CHAP user' \
'disable:Disable a user without removing the entry' \
'enable:Re-enable a previously disabled user' \
'rotate:Rotate a user'\''s password with a grace window' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl iscsi users commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__add_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi users add commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__disable_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi users disable commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__enable_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi users enable commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help_commands() {
    local commands; commands=(
'list:List every CHAP user' \
'add:Add a new CHAP user' \
'remove:Remove a CHAP user' \
'disable:Disable a user without removing the entry' \
'enable:Re-enable a previously disabled user' \
'rotate:Rotate a user'\''s password with a grace window' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl iscsi users help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__add_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__add_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi users help add commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__disable_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__disable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi users help disable commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__enable_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__enable_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi users help enable commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi users help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__list_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi users help list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__remove_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi users help remove commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__rotate_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi users help rotate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__list_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi users list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__remove_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__remove_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi users remove commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__iscsi__subcmd__users__subcmd__rotate_commands] )) ||
_thurvtl__subcmd__iscsi__subcmd__users__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl iscsi users rotate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library_commands] )) ||
_thurvtl__subcmd__library_commands() {
    local commands; commands=(
'info:Show library information' \
'bounds:Show min / current / max for num_slots and num_drives' \
'restore:Restore cartridges from a cloud backend after disaster recovery' \
'restore-archive:Pull a frozen archive back into a live cartridge' \
'monitor:Monitor library activity in real-time' \
'self-test:Run the SPC-4 self-test against the changer LUN' \
'partition:Manage logical partitions (chassis-assembly, daemon-down)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl library commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__bounds_commands] )) ||
_thurvtl__subcmd__library__subcmd__bounds_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library bounds commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__help_commands] )) ||
_thurvtl__subcmd__library__subcmd__help_commands() {
    local commands; commands=(
'info:Show library information' \
'bounds:Show min / current / max for num_slots and num_drives' \
'restore:Restore cartridges from a cloud backend after disaster recovery' \
'restore-archive:Pull a frozen archive back into a live cartridge' \
'monitor:Monitor library activity in real-time' \
'self-test:Run the SPC-4 self-test against the changer LUN' \
'partition:Manage logical partitions (chassis-assembly, daemon-down)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl library help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__help__subcmd__bounds_commands] )) ||
_thurvtl__subcmd__library__subcmd__help__subcmd__bounds_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library help bounds commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__library__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__help__subcmd__info_commands] )) ||
_thurvtl__subcmd__library__subcmd__help__subcmd__info_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library help info commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__help__subcmd__monitor_commands] )) ||
_thurvtl__subcmd__library__subcmd__help__subcmd__monitor_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library help monitor commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__help__subcmd__partition_commands] )) ||
_thurvtl__subcmd__library__subcmd__help__subcmd__partition_commands() {
    local commands; commands=(
'list:List defined partitions' \
'create:Create a new partition' \
'modify:Modify an existing partition' \
'delete:Delete a partition' \
    )
    _describe -t commands 'thurvtl library help partition commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__create_commands] )) ||
_thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library help partition create commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__delete_commands] )) ||
_thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__delete_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library help partition delete commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__list_commands] )) ||
_thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library help partition list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__modify_commands] )) ||
_thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__modify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library help partition modify commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__help__subcmd__restore_commands] )) ||
_thurvtl__subcmd__library__subcmd__help__subcmd__restore_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library help restore commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__help__subcmd__restore-archive_commands] )) ||
_thurvtl__subcmd__library__subcmd__help__subcmd__restore-archive_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library help restore-archive commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__help__subcmd__self-test_commands] )) ||
_thurvtl__subcmd__library__subcmd__help__subcmd__self-test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library help self-test commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__info_commands] )) ||
_thurvtl__subcmd__library__subcmd__info_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library info commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__monitor_commands] )) ||
_thurvtl__subcmd__library__subcmd__monitor_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library monitor commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__partition_commands] )) ||
_thurvtl__subcmd__library__subcmd__partition_commands() {
    local commands; commands=(
'list:List defined partitions' \
'create:Create a new partition' \
'modify:Modify an existing partition' \
'delete:Delete a partition' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl library partition commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__partition__subcmd__create_commands] )) ||
_thurvtl__subcmd__library__subcmd__partition__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library partition create commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__partition__subcmd__delete_commands] )) ||
_thurvtl__subcmd__library__subcmd__partition__subcmd__delete_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library partition delete commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__partition__subcmd__help_commands] )) ||
_thurvtl__subcmd__library__subcmd__partition__subcmd__help_commands() {
    local commands; commands=(
'list:List defined partitions' \
'create:Create a new partition' \
'modify:Modify an existing partition' \
'delete:Delete a partition' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl library partition help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__create_commands] )) ||
_thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__create_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library partition help create commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__delete_commands] )) ||
_thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__delete_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library partition help delete commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library partition help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__list_commands] )) ||
_thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library partition help list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__modify_commands] )) ||
_thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__modify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library partition help modify commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__partition__subcmd__list_commands] )) ||
_thurvtl__subcmd__library__subcmd__partition__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library partition list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__partition__subcmd__modify_commands] )) ||
_thurvtl__subcmd__library__subcmd__partition__subcmd__modify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library partition modify commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__restore_commands] )) ||
_thurvtl__subcmd__library__subcmd__restore_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library restore commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__restore-archive_commands] )) ||
_thurvtl__subcmd__library__subcmd__restore-archive_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library restore-archive commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__library__subcmd__self-test_commands] )) ||
_thurvtl__subcmd__library__subcmd__self-test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl library self-test commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system_commands] )) ||
_thurvtl__subcmd__system_commands() {
    local commands; commands=(
'gc:Garbage-collect orphan chunks from the chunk pool' \
'audit:Audit-chain operations' \
'storage:Storage-backend operations' \
'stats:Dedup ratio, per-cartridge contribution, HEAD-skip rate' \
'daemon-health:Probe the daemon'\''s admin Unix socket' \
'monitor:Live activity screen — holds and redraws ~1s, Ctrl-C to exit' \
'reset-stats:Reset all activity stats to their initial state' \
'verify:Library-wide consistency check' \
'tiering:Cartridge tiering — evaluate placement policies' \
'regenerate-cert:Regenerate the admin HTTP self-signed TLS cert' \
'set-admin-password:Set the web-admin (Web UI) password' \
'alerting:First-party alerting (email + webhook)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl system commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__alerting_commands] )) ||
_thurvtl__subcmd__system__subcmd__alerting_commands() {
    local commands; commands=(
'list:Show configured alert sinks and dedup window' \
'test:Fire a synthetic alert through one sink' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl system alerting commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__alerting__subcmd__help_commands] )) ||
_thurvtl__subcmd__system__subcmd__alerting__subcmd__help_commands() {
    local commands; commands=(
'list:Show configured alert sinks and dedup window' \
'test:Fire a synthetic alert through one sink' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl system alerting help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__alerting__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__system__subcmd__alerting__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system alerting help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__alerting__subcmd__help__subcmd__list_commands] )) ||
_thurvtl__subcmd__system__subcmd__alerting__subcmd__help__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system alerting help list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__alerting__subcmd__help__subcmd__test_commands] )) ||
_thurvtl__subcmd__system__subcmd__alerting__subcmd__help__subcmd__test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system alerting help test commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__alerting__subcmd__list_commands] )) ||
_thurvtl__subcmd__system__subcmd__alerting__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system alerting list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__alerting__subcmd__test_commands] )) ||
_thurvtl__subcmd__system__subcmd__alerting__subcmd__test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system alerting test commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__audit_commands] )) ||
_thurvtl__subcmd__system__subcmd__audit_commands() {
    local commands; commands=(
'tail:Print recent audit entries (optionally follow with -f)' \
'export:Export entries in the requested date range' \
'verify:Verify the tamper-evident chain end-to-end' \
'verify-offline:Offline-verify a copy of an audit directory' \
'rotate:Operator-acknowledged chain reset after a verify failure' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl system audit commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__audit__subcmd__export_commands] )) ||
_thurvtl__subcmd__system__subcmd__audit__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system audit export commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__audit__subcmd__help_commands] )) ||
_thurvtl__subcmd__system__subcmd__audit__subcmd__help_commands() {
    local commands; commands=(
'tail:Print recent audit entries (optionally follow with -f)' \
'export:Export entries in the requested date range' \
'verify:Verify the tamper-evident chain end-to-end' \
'verify-offline:Offline-verify a copy of an audit directory' \
'rotate:Operator-acknowledged chain reset after a verify failure' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl system audit help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__export_commands] )) ||
_thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system audit help export commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system audit help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__rotate_commands] )) ||
_thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system audit help rotate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__tail_commands] )) ||
_thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__tail_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system audit help tail commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify_commands] )) ||
_thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system audit help verify commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify-offline_commands] )) ||
_thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify-offline_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system audit help verify-offline commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__audit__subcmd__rotate_commands] )) ||
_thurvtl__subcmd__system__subcmd__audit__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system audit rotate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__audit__subcmd__tail_commands] )) ||
_thurvtl__subcmd__system__subcmd__audit__subcmd__tail_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system audit tail commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__audit__subcmd__verify_commands] )) ||
_thurvtl__subcmd__system__subcmd__audit__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system audit verify commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__audit__subcmd__verify-offline_commands] )) ||
_thurvtl__subcmd__system__subcmd__audit__subcmd__verify-offline_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system audit verify-offline commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__daemon-health_commands] )) ||
_thurvtl__subcmd__system__subcmd__daemon-health_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system daemon-health commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__gc_commands] )) ||
_thurvtl__subcmd__system__subcmd__gc_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system gc commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help_commands] )) ||
_thurvtl__subcmd__system__subcmd__help_commands() {
    local commands; commands=(
'gc:Garbage-collect orphan chunks from the chunk pool' \
'audit:Audit-chain operations' \
'storage:Storage-backend operations' \
'stats:Dedup ratio, per-cartridge contribution, HEAD-skip rate' \
'daemon-health:Probe the daemon'\''s admin Unix socket' \
'monitor:Live activity screen — holds and redraws ~1s, Ctrl-C to exit' \
'reset-stats:Reset all activity stats to their initial state' \
'verify:Library-wide consistency check' \
'tiering:Cartridge tiering — evaluate placement policies' \
'regenerate-cert:Regenerate the admin HTTP self-signed TLS cert' \
'set-admin-password:Set the web-admin (Web UI) password' \
'alerting:First-party alerting (email + webhook)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl system help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__alerting_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__alerting_commands() {
    local commands; commands=(
'list:Show configured alert sinks and dedup window' \
'test:Fire a synthetic alert through one sink' \
    )
    _describe -t commands 'thurvtl system help alerting commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__alerting__subcmd__list_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__alerting__subcmd__list_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help alerting list commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__alerting__subcmd__test_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__alerting__subcmd__test_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help alerting test commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__audit_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__audit_commands() {
    local commands; commands=(
'tail:Print recent audit entries (optionally follow with -f)' \
'export:Export entries in the requested date range' \
'verify:Verify the tamper-evident chain end-to-end' \
'verify-offline:Offline-verify a copy of an audit directory' \
'rotate:Operator-acknowledged chain reset after a verify failure' \
    )
    _describe -t commands 'thurvtl system help audit commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__export_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__export_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help audit export commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__rotate_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__rotate_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help audit rotate commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__tail_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__tail_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help audit tail commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help audit verify commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify-offline_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify-offline_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help audit verify-offline commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__daemon-health_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__daemon-health_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help daemon-health commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__gc_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__gc_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help gc commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__monitor_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__monitor_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help monitor commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__regenerate-cert_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__regenerate-cert_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help regenerate-cert commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__reset-stats_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__reset-stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help reset-stats commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__set-admin-password_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__set-admin-password_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help set-admin-password commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__stats_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help stats commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__storage_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__storage_commands() {
    local commands; commands=(
'check:Check storage-backend connectivity, auth, and read/write/delete' \
'benchmark:First-party storage-backend throughput benchmark (daemon-down)' \
    )
    _describe -t commands 'thurvtl system help storage commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__storage__subcmd__benchmark_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__storage__subcmd__benchmark_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help storage benchmark commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__storage__subcmd__check_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__storage__subcmd__check_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help storage check commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__tiering_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__tiering_commands() {
    local commands; commands=(
'plan:Show migrations the tiering policies would trigger' \
'run-now:Apply the tiering plan now (migrates cartridges)' \
'status:Summarize tiering\: policy count and pending moves' \
    )
    _describe -t commands 'thurvtl system help tiering commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__tiering__subcmd__plan_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__tiering__subcmd__plan_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help tiering plan commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__tiering__subcmd__run-now_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__tiering__subcmd__run-now_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help tiering run-now commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__tiering__subcmd__status_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__tiering__subcmd__status_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help tiering status commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__help__subcmd__verify_commands] )) ||
_thurvtl__subcmd__system__subcmd__help__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system help verify commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__monitor_commands] )) ||
_thurvtl__subcmd__system__subcmd__monitor_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system monitor commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__regenerate-cert_commands] )) ||
_thurvtl__subcmd__system__subcmd__regenerate-cert_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system regenerate-cert commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__reset-stats_commands] )) ||
_thurvtl__subcmd__system__subcmd__reset-stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system reset-stats commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__set-admin-password_commands] )) ||
_thurvtl__subcmd__system__subcmd__set-admin-password_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system set-admin-password commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__stats_commands] )) ||
_thurvtl__subcmd__system__subcmd__stats_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system stats commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__storage_commands] )) ||
_thurvtl__subcmd__system__subcmd__storage_commands() {
    local commands; commands=(
'check:Check storage-backend connectivity, auth, and read/write/delete' \
'benchmark:First-party storage-backend throughput benchmark (daemon-down)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl system storage commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__storage__subcmd__benchmark_commands] )) ||
_thurvtl__subcmd__system__subcmd__storage__subcmd__benchmark_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system storage benchmark commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__storage__subcmd__check_commands] )) ||
_thurvtl__subcmd__system__subcmd__storage__subcmd__check_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system storage check commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__storage__subcmd__help_commands] )) ||
_thurvtl__subcmd__system__subcmd__storage__subcmd__help_commands() {
    local commands; commands=(
'check:Check storage-backend connectivity, auth, and read/write/delete' \
'benchmark:First-party storage-backend throughput benchmark (daemon-down)' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl system storage help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__storage__subcmd__help__subcmd__benchmark_commands] )) ||
_thurvtl__subcmd__system__subcmd__storage__subcmd__help__subcmd__benchmark_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system storage help benchmark commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__storage__subcmd__help__subcmd__check_commands] )) ||
_thurvtl__subcmd__system__subcmd__storage__subcmd__help__subcmd__check_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system storage help check commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__storage__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__system__subcmd__storage__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system storage help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__tiering_commands] )) ||
_thurvtl__subcmd__system__subcmd__tiering_commands() {
    local commands; commands=(
'plan:Show migrations the tiering policies would trigger' \
'run-now:Apply the tiering plan now (migrates cartridges)' \
'status:Summarize tiering\: policy count and pending moves' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl system tiering commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__tiering__subcmd__help_commands] )) ||
_thurvtl__subcmd__system__subcmd__tiering__subcmd__help_commands() {
    local commands; commands=(
'plan:Show migrations the tiering policies would trigger' \
'run-now:Apply the tiering plan now (migrates cartridges)' \
'status:Summarize tiering\: policy count and pending moves' \
'help:Print this message or the help of the given subcommand(s)' \
    )
    _describe -t commands 'thurvtl system tiering help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__help_commands] )) ||
_thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__help_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system tiering help help commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__plan_commands] )) ||
_thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__plan_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system tiering help plan commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__run-now_commands] )) ||
_thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__run-now_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system tiering help run-now commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__status_commands] )) ||
_thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__status_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system tiering help status commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__tiering__subcmd__plan_commands] )) ||
_thurvtl__subcmd__system__subcmd__tiering__subcmd__plan_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system tiering plan commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__tiering__subcmd__run-now_commands] )) ||
_thurvtl__subcmd__system__subcmd__tiering__subcmd__run-now_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system tiering run-now commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__tiering__subcmd__status_commands] )) ||
_thurvtl__subcmd__system__subcmd__tiering__subcmd__status_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system tiering status commands' commands "$@"
}
(( $+functions[_thurvtl__subcmd__system__subcmd__verify_commands] )) ||
_thurvtl__subcmd__system__subcmd__verify_commands() {
    local commands; commands=()
    _describe -t commands 'thurvtl system verify commands' commands "$@"
}

if [ "$funcstack[1]" = "_thurvtl" ]; then
    _thurvtl "$@"
else
    compdef _thurvtl thurvtl
fi
