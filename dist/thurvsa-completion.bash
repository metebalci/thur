_thurvsa() {
    local i cur prev opts cmd
    COMPREPLY=()
    if [[ "${BASH_VERSINFO[0]}" -ge 4 ]]; then
        cur="$2"
    else
        cur="${COMP_WORDS[COMP_CWORD]}"
    fi
    prev="$3"
    cmd=""
    opts=""

    for i in "${COMP_WORDS[@]:0:COMP_CWORD}"
    do
        case "${cmd},${i}" in
            ",$1")
                cmd="thurvsa"
                ;;
            thurvsa,config)
                cmd="thurvsa__subcmd__config"
                ;;
            thurvsa,help)
                cmd="thurvsa__subcmd__help"
                ;;
            thurvsa,iscsi)
                cmd="thurvsa__subcmd__iscsi"
                ;;
            thurvsa,nvmetcp)
                cmd="thurvsa__subcmd__nvmetcp"
                ;;
            thurvsa,system)
                cmd="thurvsa__subcmd__system"
                ;;
            thurvsa,volume)
                cmd="thurvsa__subcmd__volume"
                ;;
            thurvsa__subcmd__config,completion)
                cmd="thurvsa__subcmd__config__subcmd__completion"
                ;;
            thurvsa__subcmd__config,defaults)
                cmd="thurvsa__subcmd__config__subcmd__defaults"
                ;;
            thurvsa__subcmd__config,help)
                cmd="thurvsa__subcmd__config__subcmd__help"
                ;;
            thurvsa__subcmd__config,systemd-unit)
                cmd="thurvsa__subcmd__config__subcmd__systemd__subcmd__unit"
                ;;
            thurvsa__subcmd__config__subcmd__help,completion)
                cmd="thurvsa__subcmd__config__subcmd__help__subcmd__completion"
                ;;
            thurvsa__subcmd__config__subcmd__help,defaults)
                cmd="thurvsa__subcmd__config__subcmd__help__subcmd__defaults"
                ;;
            thurvsa__subcmd__config__subcmd__help,help)
                cmd="thurvsa__subcmd__config__subcmd__help__subcmd__help"
                ;;
            thurvsa__subcmd__config__subcmd__help,systemd-unit)
                cmd="thurvsa__subcmd__config__subcmd__help__subcmd__systemd__subcmd__unit"
                ;;
            thurvsa__subcmd__help,config)
                cmd="thurvsa__subcmd__help__subcmd__config"
                ;;
            thurvsa__subcmd__help,help)
                cmd="thurvsa__subcmd__help__subcmd__help"
                ;;
            thurvsa__subcmd__help,iscsi)
                cmd="thurvsa__subcmd__help__subcmd__iscsi"
                ;;
            thurvsa__subcmd__help,nvmetcp)
                cmd="thurvsa__subcmd__help__subcmd__nvmetcp"
                ;;
            thurvsa__subcmd__help,system)
                cmd="thurvsa__subcmd__help__subcmd__system"
                ;;
            thurvsa__subcmd__help,volume)
                cmd="thurvsa__subcmd__help__subcmd__volume"
                ;;
            thurvsa__subcmd__help__subcmd__config,completion)
                cmd="thurvsa__subcmd__help__subcmd__config__subcmd__completion"
                ;;
            thurvsa__subcmd__help__subcmd__config,defaults)
                cmd="thurvsa__subcmd__help__subcmd__config__subcmd__defaults"
                ;;
            thurvsa__subcmd__help__subcmd__config,systemd-unit)
                cmd="thurvsa__subcmd__help__subcmd__config__subcmd__systemd__subcmd__unit"
                ;;
            thurvsa__subcmd__help__subcmd__iscsi,target)
                cmd="thurvsa__subcmd__help__subcmd__iscsi__subcmd__target"
                ;;
            thurvsa__subcmd__help__subcmd__iscsi,users)
                cmd="thurvsa__subcmd__help__subcmd__iscsi__subcmd__users"
                ;;
            thurvsa__subcmd__help__subcmd__iscsi__subcmd__target,clear)
                cmd="thurvsa__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__clear"
                ;;
            thurvsa__subcmd__help__subcmd__iscsi__subcmd__target,set)
                cmd="thurvsa__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__set"
                ;;
            thurvsa__subcmd__help__subcmd__iscsi__subcmd__target,show)
                cmd="thurvsa__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__show"
                ;;
            thurvsa__subcmd__help__subcmd__iscsi__subcmd__users,add)
                cmd="thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__add"
                ;;
            thurvsa__subcmd__help__subcmd__iscsi__subcmd__users,disable)
                cmd="thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__disable"
                ;;
            thurvsa__subcmd__help__subcmd__iscsi__subcmd__users,enable)
                cmd="thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__enable"
                ;;
            thurvsa__subcmd__help__subcmd__iscsi__subcmd__users,list)
                cmd="thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__list"
                ;;
            thurvsa__subcmd__help__subcmd__iscsi__subcmd__users,remove)
                cmd="thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__remove"
                ;;
            thurvsa__subcmd__help__subcmd__iscsi__subcmd__users,rotate)
                cmd="thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__rotate"
                ;;
            thurvsa__subcmd__help__subcmd__nvmetcp,psks)
                cmd="thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks"
                ;;
            thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks,add)
                cmd="thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__add"
                ;;
            thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks,disable)
                cmd="thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__disable"
                ;;
            thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks,enable)
                cmd="thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__enable"
                ;;
            thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks,list)
                cmd="thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__list"
                ;;
            thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks,remove)
                cmd="thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__remove"
                ;;
            thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks,rotate)
                cmd="thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__rotate"
                ;;
            thurvsa__subcmd__help__subcmd__system,alerting)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__alerting"
                ;;
            thurvsa__subcmd__help__subcmd__system,audit)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__audit"
                ;;
            thurvsa__subcmd__help__subcmd__system,cloud)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__cloud"
                ;;
            thurvsa__subcmd__help__subcmd__system,daemon-health)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__daemon__subcmd__health"
                ;;
            thurvsa__subcmd__help__subcmd__system,gc)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__gc"
                ;;
            thurvsa__subcmd__help__subcmd__system,regenerate-cert)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__regenerate__subcmd__cert"
                ;;
            thurvsa__subcmd__help__subcmd__system,stats)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__stats"
                ;;
            thurvsa__subcmd__help__subcmd__system,verify)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__verify"
                ;;
            thurvsa__subcmd__help__subcmd__system__subcmd__alerting,list)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__alerting__subcmd__list"
                ;;
            thurvsa__subcmd__help__subcmd__system__subcmd__alerting,test)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__alerting__subcmd__test"
                ;;
            thurvsa__subcmd__help__subcmd__system__subcmd__audit,export)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__export"
                ;;
            thurvsa__subcmd__help__subcmd__system__subcmd__audit,rotate)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__rotate"
                ;;
            thurvsa__subcmd__help__subcmd__system__subcmd__audit,tail)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__tail"
                ;;
            thurvsa__subcmd__help__subcmd__system__subcmd__audit,verify)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify"
                ;;
            thurvsa__subcmd__help__subcmd__system__subcmd__audit,verify-offline)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify__subcmd__offline"
                ;;
            thurvsa__subcmd__help__subcmd__system__subcmd__cloud,benchmark)
                cmd="thurvsa__subcmd__help__subcmd__system__subcmd__cloud__subcmd__benchmark"
                ;;
            thurvsa__subcmd__help__subcmd__volume,create)
                cmd="thurvsa__subcmd__help__subcmd__volume__subcmd__create"
                ;;
            thurvsa__subcmd__help__subcmd__volume,destroy)
                cmd="thurvsa__subcmd__help__subcmd__volume__subcmd__destroy"
                ;;
            thurvsa__subcmd__help__subcmd__volume,info)
                cmd="thurvsa__subcmd__help__subcmd__volume__subcmd__info"
                ;;
            thurvsa__subcmd__help__subcmd__volume,key)
                cmd="thurvsa__subcmd__help__subcmd__volume__subcmd__key"
                ;;
            thurvsa__subcmd__help__subcmd__volume,list)
                cmd="thurvsa__subcmd__help__subcmd__volume__subcmd__list"
                ;;
            thurvsa__subcmd__help__subcmd__volume,modify)
                cmd="thurvsa__subcmd__help__subcmd__volume__subcmd__modify"
                ;;
            thurvsa__subcmd__help__subcmd__volume__subcmd__key,export)
                cmd="thurvsa__subcmd__help__subcmd__volume__subcmd__key__subcmd__export"
                ;;
            thurvsa__subcmd__help__subcmd__volume__subcmd__key,import)
                cmd="thurvsa__subcmd__help__subcmd__volume__subcmd__key__subcmd__import"
                ;;
            thurvsa__subcmd__help__subcmd__volume__subcmd__key,migrate)
                cmd="thurvsa__subcmd__help__subcmd__volume__subcmd__key__subcmd__migrate"
                ;;
            thurvsa__subcmd__iscsi,help)
                cmd="thurvsa__subcmd__iscsi__subcmd__help"
                ;;
            thurvsa__subcmd__iscsi,target)
                cmd="thurvsa__subcmd__iscsi__subcmd__target"
                ;;
            thurvsa__subcmd__iscsi,users)
                cmd="thurvsa__subcmd__iscsi__subcmd__users"
                ;;
            thurvsa__subcmd__iscsi__subcmd__help,help)
                cmd="thurvsa__subcmd__iscsi__subcmd__help__subcmd__help"
                ;;
            thurvsa__subcmd__iscsi__subcmd__help,target)
                cmd="thurvsa__subcmd__iscsi__subcmd__help__subcmd__target"
                ;;
            thurvsa__subcmd__iscsi__subcmd__help,users)
                cmd="thurvsa__subcmd__iscsi__subcmd__help__subcmd__users"
                ;;
            thurvsa__subcmd__iscsi__subcmd__help__subcmd__target,clear)
                cmd="thurvsa__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__clear"
                ;;
            thurvsa__subcmd__iscsi__subcmd__help__subcmd__target,set)
                cmd="thurvsa__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__set"
                ;;
            thurvsa__subcmd__iscsi__subcmd__help__subcmd__target,show)
                cmd="thurvsa__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__show"
                ;;
            thurvsa__subcmd__iscsi__subcmd__help__subcmd__users,add)
                cmd="thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__add"
                ;;
            thurvsa__subcmd__iscsi__subcmd__help__subcmd__users,disable)
                cmd="thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__disable"
                ;;
            thurvsa__subcmd__iscsi__subcmd__help__subcmd__users,enable)
                cmd="thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__enable"
                ;;
            thurvsa__subcmd__iscsi__subcmd__help__subcmd__users,list)
                cmd="thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__list"
                ;;
            thurvsa__subcmd__iscsi__subcmd__help__subcmd__users,remove)
                cmd="thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__remove"
                ;;
            thurvsa__subcmd__iscsi__subcmd__help__subcmd__users,rotate)
                cmd="thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__rotate"
                ;;
            thurvsa__subcmd__iscsi__subcmd__target,clear)
                cmd="thurvsa__subcmd__iscsi__subcmd__target__subcmd__clear"
                ;;
            thurvsa__subcmd__iscsi__subcmd__target,help)
                cmd="thurvsa__subcmd__iscsi__subcmd__target__subcmd__help"
                ;;
            thurvsa__subcmd__iscsi__subcmd__target,set)
                cmd="thurvsa__subcmd__iscsi__subcmd__target__subcmd__set"
                ;;
            thurvsa__subcmd__iscsi__subcmd__target,show)
                cmd="thurvsa__subcmd__iscsi__subcmd__target__subcmd__show"
                ;;
            thurvsa__subcmd__iscsi__subcmd__target__subcmd__help,clear)
                cmd="thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__clear"
                ;;
            thurvsa__subcmd__iscsi__subcmd__target__subcmd__help,help)
                cmd="thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__help"
                ;;
            thurvsa__subcmd__iscsi__subcmd__target__subcmd__help,set)
                cmd="thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__set"
                ;;
            thurvsa__subcmd__iscsi__subcmd__target__subcmd__help,show)
                cmd="thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__show"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users,add)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__add"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users,disable)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__disable"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users,enable)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__enable"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users,help)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__help"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users,list)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__list"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users,remove)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__remove"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users,rotate)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__rotate"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users__subcmd__help,add)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__add"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users__subcmd__help,disable)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__disable"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users__subcmd__help,enable)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__enable"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users__subcmd__help,help)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__help"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users__subcmd__help,list)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__list"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users__subcmd__help,remove)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__remove"
                ;;
            thurvsa__subcmd__iscsi__subcmd__users__subcmd__help,rotate)
                cmd="thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__rotate"
                ;;
            thurvsa__subcmd__nvmetcp,help)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__help"
                ;;
            thurvsa__subcmd__nvmetcp,psks)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__help,help)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__help"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__help,psks)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks,add)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__add"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks,disable)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__disable"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks,enable)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__enable"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks,list)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__list"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks,remove)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__remove"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks,rotate)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__rotate"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks,add)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__add"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks,disable)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__disable"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks,enable)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__enable"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks,help)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks,list)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__list"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks,remove)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__remove"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks,rotate)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__rotate"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help,add)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__add"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help,disable)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__disable"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help,enable)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__enable"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help,help)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__help"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help,list)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__list"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help,remove)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__remove"
                ;;
            thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help,rotate)
                cmd="thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__rotate"
                ;;
            thurvsa__subcmd__system,alerting)
                cmd="thurvsa__subcmd__system__subcmd__alerting"
                ;;
            thurvsa__subcmd__system,audit)
                cmd="thurvsa__subcmd__system__subcmd__audit"
                ;;
            thurvsa__subcmd__system,cloud)
                cmd="thurvsa__subcmd__system__subcmd__cloud"
                ;;
            thurvsa__subcmd__system,daemon-health)
                cmd="thurvsa__subcmd__system__subcmd__daemon__subcmd__health"
                ;;
            thurvsa__subcmd__system,gc)
                cmd="thurvsa__subcmd__system__subcmd__gc"
                ;;
            thurvsa__subcmd__system,help)
                cmd="thurvsa__subcmd__system__subcmd__help"
                ;;
            thurvsa__subcmd__system,regenerate-cert)
                cmd="thurvsa__subcmd__system__subcmd__regenerate__subcmd__cert"
                ;;
            thurvsa__subcmd__system,stats)
                cmd="thurvsa__subcmd__system__subcmd__stats"
                ;;
            thurvsa__subcmd__system,verify)
                cmd="thurvsa__subcmd__system__subcmd__verify"
                ;;
            thurvsa__subcmd__system__subcmd__alerting,help)
                cmd="thurvsa__subcmd__system__subcmd__alerting__subcmd__help"
                ;;
            thurvsa__subcmd__system__subcmd__alerting,list)
                cmd="thurvsa__subcmd__system__subcmd__alerting__subcmd__list"
                ;;
            thurvsa__subcmd__system__subcmd__alerting,test)
                cmd="thurvsa__subcmd__system__subcmd__alerting__subcmd__test"
                ;;
            thurvsa__subcmd__system__subcmd__alerting__subcmd__help,help)
                cmd="thurvsa__subcmd__system__subcmd__alerting__subcmd__help__subcmd__help"
                ;;
            thurvsa__subcmd__system__subcmd__alerting__subcmd__help,list)
                cmd="thurvsa__subcmd__system__subcmd__alerting__subcmd__help__subcmd__list"
                ;;
            thurvsa__subcmd__system__subcmd__alerting__subcmd__help,test)
                cmd="thurvsa__subcmd__system__subcmd__alerting__subcmd__help__subcmd__test"
                ;;
            thurvsa__subcmd__system__subcmd__audit,export)
                cmd="thurvsa__subcmd__system__subcmd__audit__subcmd__export"
                ;;
            thurvsa__subcmd__system__subcmd__audit,help)
                cmd="thurvsa__subcmd__system__subcmd__audit__subcmd__help"
                ;;
            thurvsa__subcmd__system__subcmd__audit,rotate)
                cmd="thurvsa__subcmd__system__subcmd__audit__subcmd__rotate"
                ;;
            thurvsa__subcmd__system__subcmd__audit,tail)
                cmd="thurvsa__subcmd__system__subcmd__audit__subcmd__tail"
                ;;
            thurvsa__subcmd__system__subcmd__audit,verify)
                cmd="thurvsa__subcmd__system__subcmd__audit__subcmd__verify"
                ;;
            thurvsa__subcmd__system__subcmd__audit,verify-offline)
                cmd="thurvsa__subcmd__system__subcmd__audit__subcmd__verify__subcmd__offline"
                ;;
            thurvsa__subcmd__system__subcmd__audit__subcmd__help,export)
                cmd="thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__export"
                ;;
            thurvsa__subcmd__system__subcmd__audit__subcmd__help,help)
                cmd="thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__help"
                ;;
            thurvsa__subcmd__system__subcmd__audit__subcmd__help,rotate)
                cmd="thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__rotate"
                ;;
            thurvsa__subcmd__system__subcmd__audit__subcmd__help,tail)
                cmd="thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__tail"
                ;;
            thurvsa__subcmd__system__subcmd__audit__subcmd__help,verify)
                cmd="thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify"
                ;;
            thurvsa__subcmd__system__subcmd__audit__subcmd__help,verify-offline)
                cmd="thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify__subcmd__offline"
                ;;
            thurvsa__subcmd__system__subcmd__cloud,benchmark)
                cmd="thurvsa__subcmd__system__subcmd__cloud__subcmd__benchmark"
                ;;
            thurvsa__subcmd__system__subcmd__cloud,help)
                cmd="thurvsa__subcmd__system__subcmd__cloud__subcmd__help"
                ;;
            thurvsa__subcmd__system__subcmd__cloud__subcmd__help,benchmark)
                cmd="thurvsa__subcmd__system__subcmd__cloud__subcmd__help__subcmd__benchmark"
                ;;
            thurvsa__subcmd__system__subcmd__cloud__subcmd__help,help)
                cmd="thurvsa__subcmd__system__subcmd__cloud__subcmd__help__subcmd__help"
                ;;
            thurvsa__subcmd__system__subcmd__help,alerting)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__alerting"
                ;;
            thurvsa__subcmd__system__subcmd__help,audit)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__audit"
                ;;
            thurvsa__subcmd__system__subcmd__help,cloud)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__cloud"
                ;;
            thurvsa__subcmd__system__subcmd__help,daemon-health)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__daemon__subcmd__health"
                ;;
            thurvsa__subcmd__system__subcmd__help,gc)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__gc"
                ;;
            thurvsa__subcmd__system__subcmd__help,help)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__help"
                ;;
            thurvsa__subcmd__system__subcmd__help,regenerate-cert)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__regenerate__subcmd__cert"
                ;;
            thurvsa__subcmd__system__subcmd__help,stats)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__stats"
                ;;
            thurvsa__subcmd__system__subcmd__help,verify)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__verify"
                ;;
            thurvsa__subcmd__system__subcmd__help__subcmd__alerting,list)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__alerting__subcmd__list"
                ;;
            thurvsa__subcmd__system__subcmd__help__subcmd__alerting,test)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__alerting__subcmd__test"
                ;;
            thurvsa__subcmd__system__subcmd__help__subcmd__audit,export)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__export"
                ;;
            thurvsa__subcmd__system__subcmd__help__subcmd__audit,rotate)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__rotate"
                ;;
            thurvsa__subcmd__system__subcmd__help__subcmd__audit,tail)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__tail"
                ;;
            thurvsa__subcmd__system__subcmd__help__subcmd__audit,verify)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify"
                ;;
            thurvsa__subcmd__system__subcmd__help__subcmd__audit,verify-offline)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify__subcmd__offline"
                ;;
            thurvsa__subcmd__system__subcmd__help__subcmd__cloud,benchmark)
                cmd="thurvsa__subcmd__system__subcmd__help__subcmd__cloud__subcmd__benchmark"
                ;;
            thurvsa__subcmd__volume,create)
                cmd="thurvsa__subcmd__volume__subcmd__create"
                ;;
            thurvsa__subcmd__volume,destroy)
                cmd="thurvsa__subcmd__volume__subcmd__destroy"
                ;;
            thurvsa__subcmd__volume,help)
                cmd="thurvsa__subcmd__volume__subcmd__help"
                ;;
            thurvsa__subcmd__volume,info)
                cmd="thurvsa__subcmd__volume__subcmd__info"
                ;;
            thurvsa__subcmd__volume,key)
                cmd="thurvsa__subcmd__volume__subcmd__key"
                ;;
            thurvsa__subcmd__volume,list)
                cmd="thurvsa__subcmd__volume__subcmd__list"
                ;;
            thurvsa__subcmd__volume,modify)
                cmd="thurvsa__subcmd__volume__subcmd__modify"
                ;;
            thurvsa__subcmd__volume__subcmd__help,create)
                cmd="thurvsa__subcmd__volume__subcmd__help__subcmd__create"
                ;;
            thurvsa__subcmd__volume__subcmd__help,destroy)
                cmd="thurvsa__subcmd__volume__subcmd__help__subcmd__destroy"
                ;;
            thurvsa__subcmd__volume__subcmd__help,help)
                cmd="thurvsa__subcmd__volume__subcmd__help__subcmd__help"
                ;;
            thurvsa__subcmd__volume__subcmd__help,info)
                cmd="thurvsa__subcmd__volume__subcmd__help__subcmd__info"
                ;;
            thurvsa__subcmd__volume__subcmd__help,key)
                cmd="thurvsa__subcmd__volume__subcmd__help__subcmd__key"
                ;;
            thurvsa__subcmd__volume__subcmd__help,list)
                cmd="thurvsa__subcmd__volume__subcmd__help__subcmd__list"
                ;;
            thurvsa__subcmd__volume__subcmd__help,modify)
                cmd="thurvsa__subcmd__volume__subcmd__help__subcmd__modify"
                ;;
            thurvsa__subcmd__volume__subcmd__help__subcmd__key,export)
                cmd="thurvsa__subcmd__volume__subcmd__help__subcmd__key__subcmd__export"
                ;;
            thurvsa__subcmd__volume__subcmd__help__subcmd__key,import)
                cmd="thurvsa__subcmd__volume__subcmd__help__subcmd__key__subcmd__import"
                ;;
            thurvsa__subcmd__volume__subcmd__help__subcmd__key,migrate)
                cmd="thurvsa__subcmd__volume__subcmd__help__subcmd__key__subcmd__migrate"
                ;;
            thurvsa__subcmd__volume__subcmd__key,export)
                cmd="thurvsa__subcmd__volume__subcmd__key__subcmd__export"
                ;;
            thurvsa__subcmd__volume__subcmd__key,help)
                cmd="thurvsa__subcmd__volume__subcmd__key__subcmd__help"
                ;;
            thurvsa__subcmd__volume__subcmd__key,import)
                cmd="thurvsa__subcmd__volume__subcmd__key__subcmd__import"
                ;;
            thurvsa__subcmd__volume__subcmd__key,migrate)
                cmd="thurvsa__subcmd__volume__subcmd__key__subcmd__migrate"
                ;;
            thurvsa__subcmd__volume__subcmd__key__subcmd__help,export)
                cmd="thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__export"
                ;;
            thurvsa__subcmd__volume__subcmd__key__subcmd__help,help)
                cmd="thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__help"
                ;;
            thurvsa__subcmd__volume__subcmd__key__subcmd__help,import)
                cmd="thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__import"
                ;;
            thurvsa__subcmd__volume__subcmd__key__subcmd__help,migrate)
                cmd="thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__migrate"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        thurvsa)
            opts="-c -h -V --config --user --copyright --help --version volume system iscsi nvmetcp config help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 1 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__config)
            opts="-c -h --config --user --copyright --help defaults systemd-unit completion help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__config__subcmd__completion)
            opts="-c -h --config --user --copyright --help bash elvish fish powershell zsh"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__config__subcmd__defaults)
            opts="-c -h --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__config__subcmd__help)
            opts="defaults systemd-unit completion help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__config__subcmd__help__subcmd__completion)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__config__subcmd__help__subcmd__defaults)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__config__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__config__subcmd__help__subcmd__systemd__subcmd__unit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__config__subcmd__systemd__subcmd__unit)
            opts="-c -h --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help)
            opts="volume system iscsi nvmetcp config help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__config)
            opts="defaults systemd-unit completion"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__config__subcmd__completion)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__config__subcmd__defaults)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__config__subcmd__systemd__subcmd__unit)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__iscsi)
            opts="users target"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__iscsi__subcmd__target)
            opts="show set clear"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__clear)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__set)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__iscsi__subcmd__users)
            opts="list add remove disable enable rotate"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__disable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__enable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__rotate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__nvmetcp)
            opts="psks"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks)
            opts="list add remove disable enable rotate"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__disable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__enable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__nvmetcp__subcmd__psks__subcmd__rotate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system)
            opts="cloud gc regenerate-cert alerting daemon-health audit stats verify"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__alerting)
            opts="list test"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__alerting__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__alerting__subcmd__test)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__audit)
            opts="tail export verify verify-offline rotate"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__export)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__rotate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__tail)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify__subcmd__offline)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__cloud)
            opts="benchmark"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__cloud__subcmd__benchmark)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__daemon__subcmd__health)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__gc)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__regenerate__subcmd__cert)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__stats)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__system__subcmd__verify)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__volume)
            opts="create list info destroy modify key"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__volume__subcmd__create)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__volume__subcmd__destroy)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__volume__subcmd__info)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__volume__subcmd__key)
            opts="migrate export import"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__volume__subcmd__key__subcmd__export)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__volume__subcmd__key__subcmd__import)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__volume__subcmd__key__subcmd__migrate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__volume__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__help__subcmd__volume__subcmd__modify)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi)
            opts="-c -h --config --user --copyright --help users target help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__help)
            opts="users target help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__help__subcmd__target)
            opts="show set clear"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__clear)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__set)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__help__subcmd__users)
            opts="list add remove disable enable rotate"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__disable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__enable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__rotate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__target)
            opts="-c -h --config --user --copyright --help show set clear help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__target__subcmd__clear)
            opts="-c -h --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__target__subcmd__help)
            opts="show set clear help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__clear)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__set)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__show)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__target__subcmd__set)
            opts="-c -h --username --password --password-stdin --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --username)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --password)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__target__subcmd__show)
            opts="-c -h --json --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users)
            opts="-c -h --config --user --copyright --help list add remove disable enable rotate help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__add)
            opts="-c -h --password --password-stdin --mutual-chap --partition --config --user --copyright --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --password)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --partition)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__disable)
            opts="-c -h --config --user --copyright --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__enable)
            opts="-c -h --config --user --copyright --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__help)
            opts="list add remove disable enable rotate help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__disable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__enable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__rotate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__list)
            opts="-c -h --json --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__remove)
            opts="-c -h --config --user --copyright --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__iscsi__subcmd__users__subcmd__rotate)
            opts="-c -h --password --password-stdin --grace --cancel --config --user --copyright --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --password)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --grace)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp)
            opts="-c -h --config --user --copyright --help psks help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__help)
            opts="psks help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks)
            opts="list add remove disable enable rotate"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__disable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__enable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__help__subcmd__psks__subcmd__rotate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks)
            opts="-c -h --config --user --copyright --help list add remove disable enable rotate help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__add)
            opts="-c -h --host-nqn --key --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --host-nqn)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --key)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__disable)
            opts="-c -h --host-nqn --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --host-nqn)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__enable)
            opts="-c -h --host-nqn --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --host-nqn)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help)
            opts="list add remove disable enable rotate help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__add)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__disable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__enable)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__remove)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__help__subcmd__rotate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__list)
            opts="-c -h --json --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__remove)
            opts="-c -h --host-nqn --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --host-nqn)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__nvmetcp__subcmd__psks__subcmd__rotate)
            opts="-c -h --host-nqn --key --grace --cancel --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --host-nqn)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --key)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --grace)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system)
            opts="-c -h --config --user --copyright --help cloud gc regenerate-cert alerting daemon-health audit stats verify help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__alerting)
            opts="-c -h --config --user --copyright --help list test help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__alerting__subcmd__help)
            opts="list test help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__alerting__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__alerting__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__alerting__subcmd__help__subcmd__test)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__alerting__subcmd__list)
            opts="-c -h --json --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__alerting__subcmd__test)
            opts="-c -h --severity --config --user --copyright --help <SINK>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --severity)
                    COMPREPLY=($(compgen -W "info warn error" -- "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__audit)
            opts="-c -h --config --user --copyright --help tail export verify verify-offline rotate help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__audit__subcmd__export)
            opts="-c -h --format --from --to --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --format)
                    COMPREPLY=($(compgen -W "jsonl csv" -- "${cur}"))
                    return 0
                    ;;
                --from)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --to)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__audit__subcmd__help)
            opts="tail export verify verify-offline rotate help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__export)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__rotate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__tail)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify__subcmd__offline)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__audit__subcmd__rotate)
            opts="-c -h --accept-break --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__audit__subcmd__tail)
            opts="-f -n -c -h --follow --lines --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --lines)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -n)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__audit__subcmd__verify)
            opts="-c -h --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__audit__subcmd__verify__subcmd__offline)
            opts="-c -h --dir --json --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --dir)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__cloud)
            opts="-c -h --config --user --copyright --help benchmark help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__cloud__subcmd__benchmark)
            opts="-c -h --backend --total-gb --chunk-size-mb --concurrency --chunk-size-mb-sweep --concurrency-sweep --skip-download --yes --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --backend)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --total-gb)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --chunk-size-mb)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --concurrency)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --chunk-size-mb-sweep)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --concurrency-sweep)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__cloud__subcmd__help)
            opts="benchmark help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__cloud__subcmd__help__subcmd__benchmark)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__cloud__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__daemon__subcmd__health)
            opts="-c -h --json --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__gc)
            opts="-c -h --dry-run --cloud --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help)
            opts="cloud gc regenerate-cert alerting daemon-health audit stats verify help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__alerting)
            opts="list test"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__alerting__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__alerting__subcmd__test)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__audit)
            opts="tail export verify verify-offline rotate"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__export)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__rotate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__tail)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify__subcmd__offline)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__cloud)
            opts="benchmark"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__cloud__subcmd__benchmark)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__daemon__subcmd__health)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__gc)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__regenerate__subcmd__cert)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__stats)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__help__subcmd__verify)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__regenerate__subcmd__cert)
            opts="-c -h --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__stats)
            opts="-c -h --json --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__system__subcmd__verify)
            opts="-c -h --skip-cloud --verbose --json --config --user --copyright --help [VOLUMES]..."
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume)
            opts="-c -h --config --user --copyright --help create list info destroy modify key help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 2 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__create)
            opts="-c -h --size --backend --page-size --dedup --worm --encrypt --key-file --keystore --dek-source --sync-after --lun --config --user --copyright --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --size)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --backend)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --page-size)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --dedup)
                    COMPREPLY=($(compgen -W "local global" -- "${cur}"))
                    return 0
                    ;;
                --key-file)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --keystore)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --dek-source)
                    COMPREPLY=($(compgen -W "daemon backend" -- "${cur}"))
                    return 0
                    ;;
                --sync-after)
                    COMPREPLY=($(compgen -W "cloud disk memory" -- "${cur}"))
                    return 0
                    ;;
                --lun)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__destroy)
            opts="-c -h --force --config --user --copyright --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__help)
            opts="create list info destroy modify key help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__help__subcmd__create)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__help__subcmd__destroy)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__help__subcmd__info)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__help__subcmd__key)
            opts="migrate export import"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__help__subcmd__key__subcmd__export)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__help__subcmd__key__subcmd__import)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__help__subcmd__key__subcmd__migrate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__help__subcmd__list)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__help__subcmd__modify)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__info)
            opts="-c -h --json --config --user --copyright --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__key)
            opts="-c -h --config --user --copyright --help migrate export import help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__key__subcmd__export)
            opts="-c -h --to --iter --config --user --copyright --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --to)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --iter)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__key__subcmd__help)
            opts="migrate export import help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__export)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__help)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__import)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__key__subcmd__help__subcmd__migrate)
            opts=""
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 5 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__key__subcmd__import)
            opts="-c -h --from --keystore --config --user --copyright --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --from)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --keystore)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__key__subcmd__migrate)
            opts="-c -h --to --purge-local --config --user --copyright --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --to)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__list)
            opts="-c -h --json --config --user --copyright --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
        thurvsa__subcmd__volume__subcmd__modify)
            opts="-c -h --sync-after --config --user --copyright --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --sync-after)
                    COMPREPLY=($(compgen -W "cloud disk memory" -- "${cur}"))
                    return 0
                    ;;
                --config)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                -c)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --user)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                *)
                    COMPREPLY=()
                    ;;
            esac
            COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
            return 0
            ;;
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _thurvsa -o nosort -o bashdefault -o default thurvsa
else
    complete -F _thurvsa -o bashdefault -o default thurvsa
fi
