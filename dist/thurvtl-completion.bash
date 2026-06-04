_thurvtl() {
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
                cmd="thurvtl"
                ;;
            thurvtl,cartridge)
                cmd="thurvtl__subcmd__cartridge"
                ;;
            thurvtl,changer)
                cmd="thurvtl__subcmd__changer"
                ;;
            thurvtl,config)
                cmd="thurvtl__subcmd__config"
                ;;
            thurvtl,drive)
                cmd="thurvtl__subcmd__drive"
                ;;
            thurvtl,help)
                cmd="thurvtl__subcmd__help"
                ;;
            thurvtl,iscsi)
                cmd="thurvtl__subcmd__iscsi"
                ;;
            thurvtl,library)
                cmd="thurvtl__subcmd__library"
                ;;
            thurvtl,system)
                cmd="thurvtl__subcmd__system"
                ;;
            thurvtl__subcmd__cartridge,archive)
                cmd="thurvtl__subcmd__cartridge__subcmd__archive"
                ;;
            thurvtl__subcmd__cartridge,create)
                cmd="thurvtl__subcmd__cartridge__subcmd__create"
                ;;
            thurvtl__subcmd__cartridge,export)
                cmd="thurvtl__subcmd__cartridge__subcmd__export"
                ;;
            thurvtl__subcmd__cartridge,help)
                cmd="thurvtl__subcmd__cartridge__subcmd__help"
                ;;
            thurvtl__subcmd__cartridge,import)
                cmd="thurvtl__subcmd__cartridge__subcmd__import"
                ;;
            thurvtl__subcmd__cartridge,info)
                cmd="thurvtl__subcmd__cartridge__subcmd__info"
                ;;
            thurvtl__subcmd__cartridge,key)
                cmd="thurvtl__subcmd__cartridge__subcmd__key"
                ;;
            thurvtl__subcmd__cartridge,legal-hold)
                cmd="thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold"
                ;;
            thurvtl__subcmd__cartridge,list)
                cmd="thurvtl__subcmd__cartridge__subcmd__list"
                ;;
            thurvtl__subcmd__cartridge,migrate)
                cmd="thurvtl__subcmd__cartridge__subcmd__migrate"
                ;;
            thurvtl__subcmd__cartridge,reset-stats)
                cmd="thurvtl__subcmd__cartridge__subcmd__reset__subcmd__stats"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help,archive)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__archive"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help,create)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__create"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help,export)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__export"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help,help)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help,import)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__import"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help,info)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__info"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help,key)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__key"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help,legal-hold)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal__subcmd__hold"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help,list)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__list"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help,migrate)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__migrate"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help,reset-stats)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__reset__subcmd__stats"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help__subcmd__key,migrate)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__key__subcmd__migrate"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help__subcmd__key,show)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__key__subcmd__show"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal__subcmd__hold,clear)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal__subcmd__hold__subcmd__clear"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal__subcmd__hold,set)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal__subcmd__hold__subcmd__set"
                ;;
            thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal__subcmd__hold,status)
                cmd="thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal__subcmd__hold__subcmd__status"
                ;;
            thurvtl__subcmd__cartridge__subcmd__key,help)
                cmd="thurvtl__subcmd__cartridge__subcmd__key__subcmd__help"
                ;;
            thurvtl__subcmd__cartridge__subcmd__key,migrate)
                cmd="thurvtl__subcmd__cartridge__subcmd__key__subcmd__migrate"
                ;;
            thurvtl__subcmd__cartridge__subcmd__key,show)
                cmd="thurvtl__subcmd__cartridge__subcmd__key__subcmd__show"
                ;;
            thurvtl__subcmd__cartridge__subcmd__key__subcmd__help,help)
                cmd="thurvtl__subcmd__cartridge__subcmd__key__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__cartridge__subcmd__key__subcmd__help,migrate)
                cmd="thurvtl__subcmd__cartridge__subcmd__key__subcmd__help__subcmd__migrate"
                ;;
            thurvtl__subcmd__cartridge__subcmd__key__subcmd__help,show)
                cmd="thurvtl__subcmd__cartridge__subcmd__key__subcmd__help__subcmd__show"
                ;;
            thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold,clear)
                cmd="thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__clear"
                ;;
            thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold,help)
                cmd="thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help"
                ;;
            thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold,set)
                cmd="thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__set"
                ;;
            thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold,status)
                cmd="thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__status"
                ;;
            thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help,clear)
                cmd="thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help__subcmd__clear"
                ;;
            thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help,help)
                cmd="thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help,set)
                cmd="thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help__subcmd__set"
                ;;
            thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help,status)
                cmd="thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help__subcmd__status"
                ;;
            thurvtl__subcmd__changer,help)
                cmd="thurvtl__subcmd__changer__subcmd__help"
                ;;
            thurvtl__subcmd__changer,inventory)
                cmd="thurvtl__subcmd__changer__subcmd__inventory"
                ;;
            thurvtl__subcmd__changer,load)
                cmd="thurvtl__subcmd__changer__subcmd__load"
                ;;
            thurvtl__subcmd__changer,move)
                cmd="thurvtl__subcmd__changer__subcmd__move"
                ;;
            thurvtl__subcmd__changer,unload)
                cmd="thurvtl__subcmd__changer__subcmd__unload"
                ;;
            thurvtl__subcmd__changer__subcmd__help,help)
                cmd="thurvtl__subcmd__changer__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__changer__subcmd__help,inventory)
                cmd="thurvtl__subcmd__changer__subcmd__help__subcmd__inventory"
                ;;
            thurvtl__subcmd__changer__subcmd__help,load)
                cmd="thurvtl__subcmd__changer__subcmd__help__subcmd__load"
                ;;
            thurvtl__subcmd__changer__subcmd__help,move)
                cmd="thurvtl__subcmd__changer__subcmd__help__subcmd__move"
                ;;
            thurvtl__subcmd__changer__subcmd__help,unload)
                cmd="thurvtl__subcmd__changer__subcmd__help__subcmd__unload"
                ;;
            thurvtl__subcmd__config,completion)
                cmd="thurvtl__subcmd__config__subcmd__completion"
                ;;
            thurvtl__subcmd__config,defaults)
                cmd="thurvtl__subcmd__config__subcmd__defaults"
                ;;
            thurvtl__subcmd__config,help)
                cmd="thurvtl__subcmd__config__subcmd__help"
                ;;
            thurvtl__subcmd__config,systemd-unit)
                cmd="thurvtl__subcmd__config__subcmd__systemd__subcmd__unit"
                ;;
            thurvtl__subcmd__config__subcmd__help,completion)
                cmd="thurvtl__subcmd__config__subcmd__help__subcmd__completion"
                ;;
            thurvtl__subcmd__config__subcmd__help,defaults)
                cmd="thurvtl__subcmd__config__subcmd__help__subcmd__defaults"
                ;;
            thurvtl__subcmd__config__subcmd__help,help)
                cmd="thurvtl__subcmd__config__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__config__subcmd__help,systemd-unit)
                cmd="thurvtl__subcmd__config__subcmd__help__subcmd__systemd__subcmd__unit"
                ;;
            thurvtl__subcmd__drive,help)
                cmd="thurvtl__subcmd__drive__subcmd__help"
                ;;
            thurvtl__subcmd__drive,reset-stats)
                cmd="thurvtl__subcmd__drive__subcmd__reset__subcmd__stats"
                ;;
            thurvtl__subcmd__drive,self-test)
                cmd="thurvtl__subcmd__drive__subcmd__self__subcmd__test"
                ;;
            thurvtl__subcmd__drive,status)
                cmd="thurvtl__subcmd__drive__subcmd__status"
                ;;
            thurvtl__subcmd__drive__subcmd__help,help)
                cmd="thurvtl__subcmd__drive__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__drive__subcmd__help,reset-stats)
                cmd="thurvtl__subcmd__drive__subcmd__help__subcmd__reset__subcmd__stats"
                ;;
            thurvtl__subcmd__drive__subcmd__help,self-test)
                cmd="thurvtl__subcmd__drive__subcmd__help__subcmd__self__subcmd__test"
                ;;
            thurvtl__subcmd__drive__subcmd__help,status)
                cmd="thurvtl__subcmd__drive__subcmd__help__subcmd__status"
                ;;
            thurvtl__subcmd__help,cartridge)
                cmd="thurvtl__subcmd__help__subcmd__cartridge"
                ;;
            thurvtl__subcmd__help,changer)
                cmd="thurvtl__subcmd__help__subcmd__changer"
                ;;
            thurvtl__subcmd__help,config)
                cmd="thurvtl__subcmd__help__subcmd__config"
                ;;
            thurvtl__subcmd__help,drive)
                cmd="thurvtl__subcmd__help__subcmd__drive"
                ;;
            thurvtl__subcmd__help,help)
                cmd="thurvtl__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__help,iscsi)
                cmd="thurvtl__subcmd__help__subcmd__iscsi"
                ;;
            thurvtl__subcmd__help,library)
                cmd="thurvtl__subcmd__help__subcmd__library"
                ;;
            thurvtl__subcmd__help,system)
                cmd="thurvtl__subcmd__help__subcmd__system"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge,archive)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__archive"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge,create)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__create"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge,export)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__export"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge,import)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__import"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge,info)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__info"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge,key)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__key"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge,legal-hold)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal__subcmd__hold"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge,list)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__list"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge,migrate)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__migrate"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge,reset-stats)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__reset__subcmd__stats"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge__subcmd__key,migrate)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__key__subcmd__migrate"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge__subcmd__key,show)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__key__subcmd__show"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal__subcmd__hold,clear)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__clear"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal__subcmd__hold,set)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__set"
                ;;
            thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal__subcmd__hold,status)
                cmd="thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__status"
                ;;
            thurvtl__subcmd__help__subcmd__changer,inventory)
                cmd="thurvtl__subcmd__help__subcmd__changer__subcmd__inventory"
                ;;
            thurvtl__subcmd__help__subcmd__changer,load)
                cmd="thurvtl__subcmd__help__subcmd__changer__subcmd__load"
                ;;
            thurvtl__subcmd__help__subcmd__changer,move)
                cmd="thurvtl__subcmd__help__subcmd__changer__subcmd__move"
                ;;
            thurvtl__subcmd__help__subcmd__changer,unload)
                cmd="thurvtl__subcmd__help__subcmd__changer__subcmd__unload"
                ;;
            thurvtl__subcmd__help__subcmd__config,completion)
                cmd="thurvtl__subcmd__help__subcmd__config__subcmd__completion"
                ;;
            thurvtl__subcmd__help__subcmd__config,defaults)
                cmd="thurvtl__subcmd__help__subcmd__config__subcmd__defaults"
                ;;
            thurvtl__subcmd__help__subcmd__config,systemd-unit)
                cmd="thurvtl__subcmd__help__subcmd__config__subcmd__systemd__subcmd__unit"
                ;;
            thurvtl__subcmd__help__subcmd__drive,reset-stats)
                cmd="thurvtl__subcmd__help__subcmd__drive__subcmd__reset__subcmd__stats"
                ;;
            thurvtl__subcmd__help__subcmd__drive,self-test)
                cmd="thurvtl__subcmd__help__subcmd__drive__subcmd__self__subcmd__test"
                ;;
            thurvtl__subcmd__help__subcmd__drive,status)
                cmd="thurvtl__subcmd__help__subcmd__drive__subcmd__status"
                ;;
            thurvtl__subcmd__help__subcmd__iscsi,target)
                cmd="thurvtl__subcmd__help__subcmd__iscsi__subcmd__target"
                ;;
            thurvtl__subcmd__help__subcmd__iscsi,users)
                cmd="thurvtl__subcmd__help__subcmd__iscsi__subcmd__users"
                ;;
            thurvtl__subcmd__help__subcmd__iscsi__subcmd__target,clear)
                cmd="thurvtl__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__clear"
                ;;
            thurvtl__subcmd__help__subcmd__iscsi__subcmd__target,set)
                cmd="thurvtl__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__set"
                ;;
            thurvtl__subcmd__help__subcmd__iscsi__subcmd__target,show)
                cmd="thurvtl__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__show"
                ;;
            thurvtl__subcmd__help__subcmd__iscsi__subcmd__users,add)
                cmd="thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__add"
                ;;
            thurvtl__subcmd__help__subcmd__iscsi__subcmd__users,disable)
                cmd="thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__disable"
                ;;
            thurvtl__subcmd__help__subcmd__iscsi__subcmd__users,enable)
                cmd="thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__enable"
                ;;
            thurvtl__subcmd__help__subcmd__iscsi__subcmd__users,list)
                cmd="thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__list"
                ;;
            thurvtl__subcmd__help__subcmd__iscsi__subcmd__users,remove)
                cmd="thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__remove"
                ;;
            thurvtl__subcmd__help__subcmd__iscsi__subcmd__users,rotate)
                cmd="thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__rotate"
                ;;
            thurvtl__subcmd__help__subcmd__library,bounds)
                cmd="thurvtl__subcmd__help__subcmd__library__subcmd__bounds"
                ;;
            thurvtl__subcmd__help__subcmd__library,info)
                cmd="thurvtl__subcmd__help__subcmd__library__subcmd__info"
                ;;
            thurvtl__subcmd__help__subcmd__library,monitor)
                cmd="thurvtl__subcmd__help__subcmd__library__subcmd__monitor"
                ;;
            thurvtl__subcmd__help__subcmd__library,partition)
                cmd="thurvtl__subcmd__help__subcmd__library__subcmd__partition"
                ;;
            thurvtl__subcmd__help__subcmd__library,restore)
                cmd="thurvtl__subcmd__help__subcmd__library__subcmd__restore"
                ;;
            thurvtl__subcmd__help__subcmd__library,restore-archive)
                cmd="thurvtl__subcmd__help__subcmd__library__subcmd__restore__subcmd__archive"
                ;;
            thurvtl__subcmd__help__subcmd__library,self-test)
                cmd="thurvtl__subcmd__help__subcmd__library__subcmd__self__subcmd__test"
                ;;
            thurvtl__subcmd__help__subcmd__library__subcmd__partition,create)
                cmd="thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__create"
                ;;
            thurvtl__subcmd__help__subcmd__library__subcmd__partition,delete)
                cmd="thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__delete"
                ;;
            thurvtl__subcmd__help__subcmd__library__subcmd__partition,list)
                cmd="thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__list"
                ;;
            thurvtl__subcmd__help__subcmd__library__subcmd__partition,modify)
                cmd="thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__modify"
                ;;
            thurvtl__subcmd__help__subcmd__system,alerting)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__alerting"
                ;;
            thurvtl__subcmd__help__subcmd__system,audit)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__audit"
                ;;
            thurvtl__subcmd__help__subcmd__system,daemon-health)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__daemon__subcmd__health"
                ;;
            thurvtl__subcmd__help__subcmd__system,gc)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__gc"
                ;;
            thurvtl__subcmd__help__subcmd__system,monitor)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__monitor"
                ;;
            thurvtl__subcmd__help__subcmd__system,regenerate-cert)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__regenerate__subcmd__cert"
                ;;
            thurvtl__subcmd__help__subcmd__system,reset-stats)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__reset__subcmd__stats"
                ;;
            thurvtl__subcmd__help__subcmd__system,stats)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__stats"
                ;;
            thurvtl__subcmd__help__subcmd__system,storage)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__storage"
                ;;
            thurvtl__subcmd__help__subcmd__system,tiering)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__tiering"
                ;;
            thurvtl__subcmd__help__subcmd__system,verify)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__verify"
                ;;
            thurvtl__subcmd__help__subcmd__system__subcmd__alerting,list)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__alerting__subcmd__list"
                ;;
            thurvtl__subcmd__help__subcmd__system__subcmd__alerting,test)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__alerting__subcmd__test"
                ;;
            thurvtl__subcmd__help__subcmd__system__subcmd__audit,export)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__export"
                ;;
            thurvtl__subcmd__help__subcmd__system__subcmd__audit,rotate)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__rotate"
                ;;
            thurvtl__subcmd__help__subcmd__system__subcmd__audit,tail)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__tail"
                ;;
            thurvtl__subcmd__help__subcmd__system__subcmd__audit,verify)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify"
                ;;
            thurvtl__subcmd__help__subcmd__system__subcmd__audit,verify-offline)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify__subcmd__offline"
                ;;
            thurvtl__subcmd__help__subcmd__system__subcmd__storage,benchmark)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__storage__subcmd__benchmark"
                ;;
            thurvtl__subcmd__help__subcmd__system__subcmd__storage,check)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__storage__subcmd__check"
                ;;
            thurvtl__subcmd__help__subcmd__system__subcmd__tiering,plan)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__tiering__subcmd__plan"
                ;;
            thurvtl__subcmd__help__subcmd__system__subcmd__tiering,run-now)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__tiering__subcmd__run__subcmd__now"
                ;;
            thurvtl__subcmd__help__subcmd__system__subcmd__tiering,status)
                cmd="thurvtl__subcmd__help__subcmd__system__subcmd__tiering__subcmd__status"
                ;;
            thurvtl__subcmd__iscsi,help)
                cmd="thurvtl__subcmd__iscsi__subcmd__help"
                ;;
            thurvtl__subcmd__iscsi,target)
                cmd="thurvtl__subcmd__iscsi__subcmd__target"
                ;;
            thurvtl__subcmd__iscsi,users)
                cmd="thurvtl__subcmd__iscsi__subcmd__users"
                ;;
            thurvtl__subcmd__iscsi__subcmd__help,help)
                cmd="thurvtl__subcmd__iscsi__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__iscsi__subcmd__help,target)
                cmd="thurvtl__subcmd__iscsi__subcmd__help__subcmd__target"
                ;;
            thurvtl__subcmd__iscsi__subcmd__help,users)
                cmd="thurvtl__subcmd__iscsi__subcmd__help__subcmd__users"
                ;;
            thurvtl__subcmd__iscsi__subcmd__help__subcmd__target,clear)
                cmd="thurvtl__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__clear"
                ;;
            thurvtl__subcmd__iscsi__subcmd__help__subcmd__target,set)
                cmd="thurvtl__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__set"
                ;;
            thurvtl__subcmd__iscsi__subcmd__help__subcmd__target,show)
                cmd="thurvtl__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__show"
                ;;
            thurvtl__subcmd__iscsi__subcmd__help__subcmd__users,add)
                cmd="thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__add"
                ;;
            thurvtl__subcmd__iscsi__subcmd__help__subcmd__users,disable)
                cmd="thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__disable"
                ;;
            thurvtl__subcmd__iscsi__subcmd__help__subcmd__users,enable)
                cmd="thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__enable"
                ;;
            thurvtl__subcmd__iscsi__subcmd__help__subcmd__users,list)
                cmd="thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__list"
                ;;
            thurvtl__subcmd__iscsi__subcmd__help__subcmd__users,remove)
                cmd="thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__remove"
                ;;
            thurvtl__subcmd__iscsi__subcmd__help__subcmd__users,rotate)
                cmd="thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__rotate"
                ;;
            thurvtl__subcmd__iscsi__subcmd__target,clear)
                cmd="thurvtl__subcmd__iscsi__subcmd__target__subcmd__clear"
                ;;
            thurvtl__subcmd__iscsi__subcmd__target,help)
                cmd="thurvtl__subcmd__iscsi__subcmd__target__subcmd__help"
                ;;
            thurvtl__subcmd__iscsi__subcmd__target,set)
                cmd="thurvtl__subcmd__iscsi__subcmd__target__subcmd__set"
                ;;
            thurvtl__subcmd__iscsi__subcmd__target,show)
                cmd="thurvtl__subcmd__iscsi__subcmd__target__subcmd__show"
                ;;
            thurvtl__subcmd__iscsi__subcmd__target__subcmd__help,clear)
                cmd="thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__clear"
                ;;
            thurvtl__subcmd__iscsi__subcmd__target__subcmd__help,help)
                cmd="thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__iscsi__subcmd__target__subcmd__help,set)
                cmd="thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__set"
                ;;
            thurvtl__subcmd__iscsi__subcmd__target__subcmd__help,show)
                cmd="thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__show"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users,add)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__add"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users,disable)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__disable"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users,enable)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__enable"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users,help)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__help"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users,list)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__list"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users,remove)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__remove"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users,rotate)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__rotate"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users__subcmd__help,add)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__add"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users__subcmd__help,disable)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__disable"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users__subcmd__help,enable)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__enable"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users__subcmd__help,help)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users__subcmd__help,list)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__list"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users__subcmd__help,remove)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__remove"
                ;;
            thurvtl__subcmd__iscsi__subcmd__users__subcmd__help,rotate)
                cmd="thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__rotate"
                ;;
            thurvtl__subcmd__library,bounds)
                cmd="thurvtl__subcmd__library__subcmd__bounds"
                ;;
            thurvtl__subcmd__library,help)
                cmd="thurvtl__subcmd__library__subcmd__help"
                ;;
            thurvtl__subcmd__library,info)
                cmd="thurvtl__subcmd__library__subcmd__info"
                ;;
            thurvtl__subcmd__library,monitor)
                cmd="thurvtl__subcmd__library__subcmd__monitor"
                ;;
            thurvtl__subcmd__library,partition)
                cmd="thurvtl__subcmd__library__subcmd__partition"
                ;;
            thurvtl__subcmd__library,restore)
                cmd="thurvtl__subcmd__library__subcmd__restore"
                ;;
            thurvtl__subcmd__library,restore-archive)
                cmd="thurvtl__subcmd__library__subcmd__restore__subcmd__archive"
                ;;
            thurvtl__subcmd__library,self-test)
                cmd="thurvtl__subcmd__library__subcmd__self__subcmd__test"
                ;;
            thurvtl__subcmd__library__subcmd__help,bounds)
                cmd="thurvtl__subcmd__library__subcmd__help__subcmd__bounds"
                ;;
            thurvtl__subcmd__library__subcmd__help,help)
                cmd="thurvtl__subcmd__library__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__library__subcmd__help,info)
                cmd="thurvtl__subcmd__library__subcmd__help__subcmd__info"
                ;;
            thurvtl__subcmd__library__subcmd__help,monitor)
                cmd="thurvtl__subcmd__library__subcmd__help__subcmd__monitor"
                ;;
            thurvtl__subcmd__library__subcmd__help,partition)
                cmd="thurvtl__subcmd__library__subcmd__help__subcmd__partition"
                ;;
            thurvtl__subcmd__library__subcmd__help,restore)
                cmd="thurvtl__subcmd__library__subcmd__help__subcmd__restore"
                ;;
            thurvtl__subcmd__library__subcmd__help,restore-archive)
                cmd="thurvtl__subcmd__library__subcmd__help__subcmd__restore__subcmd__archive"
                ;;
            thurvtl__subcmd__library__subcmd__help,self-test)
                cmd="thurvtl__subcmd__library__subcmd__help__subcmd__self__subcmd__test"
                ;;
            thurvtl__subcmd__library__subcmd__help__subcmd__partition,create)
                cmd="thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__create"
                ;;
            thurvtl__subcmd__library__subcmd__help__subcmd__partition,delete)
                cmd="thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__delete"
                ;;
            thurvtl__subcmd__library__subcmd__help__subcmd__partition,list)
                cmd="thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__list"
                ;;
            thurvtl__subcmd__library__subcmd__help__subcmd__partition,modify)
                cmd="thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__modify"
                ;;
            thurvtl__subcmd__library__subcmd__partition,create)
                cmd="thurvtl__subcmd__library__subcmd__partition__subcmd__create"
                ;;
            thurvtl__subcmd__library__subcmd__partition,delete)
                cmd="thurvtl__subcmd__library__subcmd__partition__subcmd__delete"
                ;;
            thurvtl__subcmd__library__subcmd__partition,help)
                cmd="thurvtl__subcmd__library__subcmd__partition__subcmd__help"
                ;;
            thurvtl__subcmd__library__subcmd__partition,list)
                cmd="thurvtl__subcmd__library__subcmd__partition__subcmd__list"
                ;;
            thurvtl__subcmd__library__subcmd__partition,modify)
                cmd="thurvtl__subcmd__library__subcmd__partition__subcmd__modify"
                ;;
            thurvtl__subcmd__library__subcmd__partition__subcmd__help,create)
                cmd="thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__create"
                ;;
            thurvtl__subcmd__library__subcmd__partition__subcmd__help,delete)
                cmd="thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__delete"
                ;;
            thurvtl__subcmd__library__subcmd__partition__subcmd__help,help)
                cmd="thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__library__subcmd__partition__subcmd__help,list)
                cmd="thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__list"
                ;;
            thurvtl__subcmd__library__subcmd__partition__subcmd__help,modify)
                cmd="thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__modify"
                ;;
            thurvtl__subcmd__system,alerting)
                cmd="thurvtl__subcmd__system__subcmd__alerting"
                ;;
            thurvtl__subcmd__system,audit)
                cmd="thurvtl__subcmd__system__subcmd__audit"
                ;;
            thurvtl__subcmd__system,daemon-health)
                cmd="thurvtl__subcmd__system__subcmd__daemon__subcmd__health"
                ;;
            thurvtl__subcmd__system,gc)
                cmd="thurvtl__subcmd__system__subcmd__gc"
                ;;
            thurvtl__subcmd__system,help)
                cmd="thurvtl__subcmd__system__subcmd__help"
                ;;
            thurvtl__subcmd__system,monitor)
                cmd="thurvtl__subcmd__system__subcmd__monitor"
                ;;
            thurvtl__subcmd__system,regenerate-cert)
                cmd="thurvtl__subcmd__system__subcmd__regenerate__subcmd__cert"
                ;;
            thurvtl__subcmd__system,reset-stats)
                cmd="thurvtl__subcmd__system__subcmd__reset__subcmd__stats"
                ;;
            thurvtl__subcmd__system,stats)
                cmd="thurvtl__subcmd__system__subcmd__stats"
                ;;
            thurvtl__subcmd__system,storage)
                cmd="thurvtl__subcmd__system__subcmd__storage"
                ;;
            thurvtl__subcmd__system,tiering)
                cmd="thurvtl__subcmd__system__subcmd__tiering"
                ;;
            thurvtl__subcmd__system,verify)
                cmd="thurvtl__subcmd__system__subcmd__verify"
                ;;
            thurvtl__subcmd__system__subcmd__alerting,help)
                cmd="thurvtl__subcmd__system__subcmd__alerting__subcmd__help"
                ;;
            thurvtl__subcmd__system__subcmd__alerting,list)
                cmd="thurvtl__subcmd__system__subcmd__alerting__subcmd__list"
                ;;
            thurvtl__subcmd__system__subcmd__alerting,test)
                cmd="thurvtl__subcmd__system__subcmd__alerting__subcmd__test"
                ;;
            thurvtl__subcmd__system__subcmd__alerting__subcmd__help,help)
                cmd="thurvtl__subcmd__system__subcmd__alerting__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__system__subcmd__alerting__subcmd__help,list)
                cmd="thurvtl__subcmd__system__subcmd__alerting__subcmd__help__subcmd__list"
                ;;
            thurvtl__subcmd__system__subcmd__alerting__subcmd__help,test)
                cmd="thurvtl__subcmd__system__subcmd__alerting__subcmd__help__subcmd__test"
                ;;
            thurvtl__subcmd__system__subcmd__audit,export)
                cmd="thurvtl__subcmd__system__subcmd__audit__subcmd__export"
                ;;
            thurvtl__subcmd__system__subcmd__audit,help)
                cmd="thurvtl__subcmd__system__subcmd__audit__subcmd__help"
                ;;
            thurvtl__subcmd__system__subcmd__audit,rotate)
                cmd="thurvtl__subcmd__system__subcmd__audit__subcmd__rotate"
                ;;
            thurvtl__subcmd__system__subcmd__audit,tail)
                cmd="thurvtl__subcmd__system__subcmd__audit__subcmd__tail"
                ;;
            thurvtl__subcmd__system__subcmd__audit,verify)
                cmd="thurvtl__subcmd__system__subcmd__audit__subcmd__verify"
                ;;
            thurvtl__subcmd__system__subcmd__audit,verify-offline)
                cmd="thurvtl__subcmd__system__subcmd__audit__subcmd__verify__subcmd__offline"
                ;;
            thurvtl__subcmd__system__subcmd__audit__subcmd__help,export)
                cmd="thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__export"
                ;;
            thurvtl__subcmd__system__subcmd__audit__subcmd__help,help)
                cmd="thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__system__subcmd__audit__subcmd__help,rotate)
                cmd="thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__rotate"
                ;;
            thurvtl__subcmd__system__subcmd__audit__subcmd__help,tail)
                cmd="thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__tail"
                ;;
            thurvtl__subcmd__system__subcmd__audit__subcmd__help,verify)
                cmd="thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify"
                ;;
            thurvtl__subcmd__system__subcmd__audit__subcmd__help,verify-offline)
                cmd="thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify__subcmd__offline"
                ;;
            thurvtl__subcmd__system__subcmd__help,alerting)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__alerting"
                ;;
            thurvtl__subcmd__system__subcmd__help,audit)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__audit"
                ;;
            thurvtl__subcmd__system__subcmd__help,daemon-health)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__daemon__subcmd__health"
                ;;
            thurvtl__subcmd__system__subcmd__help,gc)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__gc"
                ;;
            thurvtl__subcmd__system__subcmd__help,help)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__system__subcmd__help,monitor)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__monitor"
                ;;
            thurvtl__subcmd__system__subcmd__help,regenerate-cert)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__regenerate__subcmd__cert"
                ;;
            thurvtl__subcmd__system__subcmd__help,reset-stats)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__reset__subcmd__stats"
                ;;
            thurvtl__subcmd__system__subcmd__help,stats)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__stats"
                ;;
            thurvtl__subcmd__system__subcmd__help,storage)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__storage"
                ;;
            thurvtl__subcmd__system__subcmd__help,tiering)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__tiering"
                ;;
            thurvtl__subcmd__system__subcmd__help,verify)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__verify"
                ;;
            thurvtl__subcmd__system__subcmd__help__subcmd__alerting,list)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__alerting__subcmd__list"
                ;;
            thurvtl__subcmd__system__subcmd__help__subcmd__alerting,test)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__alerting__subcmd__test"
                ;;
            thurvtl__subcmd__system__subcmd__help__subcmd__audit,export)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__export"
                ;;
            thurvtl__subcmd__system__subcmd__help__subcmd__audit,rotate)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__rotate"
                ;;
            thurvtl__subcmd__system__subcmd__help__subcmd__audit,tail)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__tail"
                ;;
            thurvtl__subcmd__system__subcmd__help__subcmd__audit,verify)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify"
                ;;
            thurvtl__subcmd__system__subcmd__help__subcmd__audit,verify-offline)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify__subcmd__offline"
                ;;
            thurvtl__subcmd__system__subcmd__help__subcmd__storage,benchmark)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__storage__subcmd__benchmark"
                ;;
            thurvtl__subcmd__system__subcmd__help__subcmd__storage,check)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__storage__subcmd__check"
                ;;
            thurvtl__subcmd__system__subcmd__help__subcmd__tiering,plan)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__tiering__subcmd__plan"
                ;;
            thurvtl__subcmd__system__subcmd__help__subcmd__tiering,run-now)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__tiering__subcmd__run__subcmd__now"
                ;;
            thurvtl__subcmd__system__subcmd__help__subcmd__tiering,status)
                cmd="thurvtl__subcmd__system__subcmd__help__subcmd__tiering__subcmd__status"
                ;;
            thurvtl__subcmd__system__subcmd__storage,benchmark)
                cmd="thurvtl__subcmd__system__subcmd__storage__subcmd__benchmark"
                ;;
            thurvtl__subcmd__system__subcmd__storage,check)
                cmd="thurvtl__subcmd__system__subcmd__storage__subcmd__check"
                ;;
            thurvtl__subcmd__system__subcmd__storage,help)
                cmd="thurvtl__subcmd__system__subcmd__storage__subcmd__help"
                ;;
            thurvtl__subcmd__system__subcmd__storage__subcmd__help,benchmark)
                cmd="thurvtl__subcmd__system__subcmd__storage__subcmd__help__subcmd__benchmark"
                ;;
            thurvtl__subcmd__system__subcmd__storage__subcmd__help,check)
                cmd="thurvtl__subcmd__system__subcmd__storage__subcmd__help__subcmd__check"
                ;;
            thurvtl__subcmd__system__subcmd__storage__subcmd__help,help)
                cmd="thurvtl__subcmd__system__subcmd__storage__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__system__subcmd__tiering,help)
                cmd="thurvtl__subcmd__system__subcmd__tiering__subcmd__help"
                ;;
            thurvtl__subcmd__system__subcmd__tiering,plan)
                cmd="thurvtl__subcmd__system__subcmd__tiering__subcmd__plan"
                ;;
            thurvtl__subcmd__system__subcmd__tiering,run-now)
                cmd="thurvtl__subcmd__system__subcmd__tiering__subcmd__run__subcmd__now"
                ;;
            thurvtl__subcmd__system__subcmd__tiering,status)
                cmd="thurvtl__subcmd__system__subcmd__tiering__subcmd__status"
                ;;
            thurvtl__subcmd__system__subcmd__tiering__subcmd__help,help)
                cmd="thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__help"
                ;;
            thurvtl__subcmd__system__subcmd__tiering__subcmd__help,plan)
                cmd="thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__plan"
                ;;
            thurvtl__subcmd__system__subcmd__tiering__subcmd__help,run-now)
                cmd="thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__run__subcmd__now"
                ;;
            thurvtl__subcmd__system__subcmd__tiering__subcmd__help,status)
                cmd="thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__status"
                ;;
            *)
                ;;
        esac
    done

    case "${cmd}" in
        thurvtl)
            opts="-c -h -V --config --user --help --version library cartridge changer drive system iscsi config help"
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
        thurvtl__subcmd__cartridge)
            opts="-c -h --config --user --help create archive migrate import export list info reset-stats legal-hold key help"
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
        thurvtl__subcmd__cartridge__subcmd__archive)
            opts="-c -h --target-backend --label --dry-run --config --user --help <BARCODE>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --target-backend)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --label)
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
        thurvtl__subcmd__cartridge__subcmd__create)
            opts="-c -h --lto-generation --chunk-size-mb --chunking --chunking-min-kb --chunking-max-kb --multi --backend --worm --dedup --encrypt --keystore --config --user --help <BARCODE>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --lto-generation)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --chunk-size-mb)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --chunking)
                    COMPREPLY=($(compgen -W "fastcdc fixed" -- "${cur}"))
                    return 0
                    ;;
                --chunking-min-kb)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --chunking-max-kb)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --multi)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --backend)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --dedup)
                    COMPREPLY=($(compgen -W "local global" -- "${cur}"))
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
        thurvtl__subcmd__cartridge__subcmd__export)
            opts="-c -h --config --user --help <SLOT> <PATH>"
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
        thurvtl__subcmd__cartridge__subcmd__help)
            opts="create archive migrate import export list info reset-stats legal-hold key help"
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__archive)
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__create)
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__export)
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__import)
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__info)
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__key)
            opts="migrate show"
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__key__subcmd__migrate)
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__key__subcmd__show)
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal__subcmd__hold)
            opts="set clear status"
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal__subcmd__hold__subcmd__clear)
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal__subcmd__hold__subcmd__set)
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__legal__subcmd__hold__subcmd__status)
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__list)
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__migrate)
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
        thurvtl__subcmd__cartridge__subcmd__help__subcmd__reset__subcmd__stats)
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
        thurvtl__subcmd__cartridge__subcmd__import)
            opts="-c -h --config --user --help <PATH> <SLOT>"
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
        thurvtl__subcmd__cartridge__subcmd__info)
            opts="-c -h --json --config --user --help <IDENTIFIER>"
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
        thurvtl__subcmd__cartridge__subcmd__key)
            opts="-c -h --config --user --help migrate show help"
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
        thurvtl__subcmd__cartridge__subcmd__key__subcmd__help)
            opts="migrate show help"
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
        thurvtl__subcmd__cartridge__subcmd__key__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__cartridge__subcmd__key__subcmd__help__subcmd__migrate)
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
        thurvtl__subcmd__cartridge__subcmd__key__subcmd__help__subcmd__show)
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
        thurvtl__subcmd__cartridge__subcmd__key__subcmd__migrate)
            opts="-c -h --to --purge-local --config --user --help <BARCODE>"
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
        thurvtl__subcmd__cartridge__subcmd__key__subcmd__show)
            opts="-c -h --config --user --help <BARCODE>"
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
        thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold)
            opts="-c -h --config --user --help set clear status help"
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
        thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__clear)
            opts="-c -h --id --reason --config --user --help <BARCODE>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --reason)
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
        thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help)
            opts="set clear status help"
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
        thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help__subcmd__clear)
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
        thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help__subcmd__set)
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
        thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__help__subcmd__status)
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
        thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__set)
            opts="-c -h --id --reason --config --user --help <BARCODE>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --id)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --reason)
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
        thurvtl__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__status)
            opts="-c -h --full --config --user --help <BARCODE>"
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
        thurvtl__subcmd__cartridge__subcmd__list)
            opts="-c -h --json --config --user --help"
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
        thurvtl__subcmd__cartridge__subcmd__migrate)
            opts="-c -h --target-backend --mode --no-verify --dry-run --config --user --help <BARCODE>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --target-backend)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mode)
                    COMPREPLY=($(compgen -W "move rebind" -- "${cur}"))
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
        thurvtl__subcmd__cartridge__subcmd__reset__subcmd__stats)
            opts="-c -h --config --user --help <BARCODE>"
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
        thurvtl__subcmd__changer)
            opts="-c -h --config --user --help inventory move load unload help"
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
        thurvtl__subcmd__changer__subcmd__help)
            opts="inventory move load unload help"
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
        thurvtl__subcmd__changer__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__changer__subcmd__help__subcmd__inventory)
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
        thurvtl__subcmd__changer__subcmd__help__subcmd__load)
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
        thurvtl__subcmd__changer__subcmd__help__subcmd__move)
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
        thurvtl__subcmd__changer__subcmd__help__subcmd__unload)
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
        thurvtl__subcmd__changer__subcmd__inventory)
            opts="-c -h --filter --json --config --user --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --filter)
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
        thurvtl__subcmd__changer__subcmd__load)
            opts="-c -h --cross-partition --config --user --help <SLOT> <DRIVE>"
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
        thurvtl__subcmd__changer__subcmd__move)
            opts="-c -h --cross-partition --config --user --help <FROM_SLOT> <TO_SLOT>"
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
        thurvtl__subcmd__changer__subcmd__unload)
            opts="-c -h --force --cross-partition --config --user --help <DRIVE> [SLOT]"
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
        thurvtl__subcmd__config)
            opts="-c -h --config --user --help defaults systemd-unit completion help"
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
        thurvtl__subcmd__config__subcmd__completion)
            opts="-c -h --config --user --help bash elvish fish powershell zsh"
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
        thurvtl__subcmd__config__subcmd__defaults)
            opts="-c -h --config --user --help"
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
        thurvtl__subcmd__config__subcmd__help)
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
        thurvtl__subcmd__config__subcmd__help__subcmd__completion)
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
        thurvtl__subcmd__config__subcmd__help__subcmd__defaults)
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
        thurvtl__subcmd__config__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__config__subcmd__help__subcmd__systemd__subcmd__unit)
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
        thurvtl__subcmd__config__subcmd__systemd__subcmd__unit)
            opts="-c -h --config --user --help"
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
        thurvtl__subcmd__drive)
            opts="-c -h --config --user --help status self-test reset-stats help"
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
        thurvtl__subcmd__drive__subcmd__help)
            opts="status self-test reset-stats help"
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
        thurvtl__subcmd__drive__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__drive__subcmd__help__subcmd__reset__subcmd__stats)
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
        thurvtl__subcmd__drive__subcmd__help__subcmd__self__subcmd__test)
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
        thurvtl__subcmd__drive__subcmd__help__subcmd__status)
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
        thurvtl__subcmd__drive__subcmd__reset__subcmd__stats)
            opts="-c -h --all --config --user --help [DRIVE]"
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
        thurvtl__subcmd__drive__subcmd__self__subcmd__test)
            opts="-c -h --json --config --user --help <DRIVE>"
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
        thurvtl__subcmd__drive__subcmd__status)
            opts="-c -h --json --config --user --help <DRIVE>"
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
        thurvtl__subcmd__help)
            opts="library cartridge changer drive system iscsi config help"
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
        thurvtl__subcmd__help__subcmd__cartridge)
            opts="create archive migrate import export list info reset-stats legal-hold key"
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__archive)
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__create)
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__export)
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__import)
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__info)
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__key)
            opts="migrate show"
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__key__subcmd__migrate)
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__key__subcmd__show)
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal__subcmd__hold)
            opts="set clear status"
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__clear)
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__set)
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__legal__subcmd__hold__subcmd__status)
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__list)
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__migrate)
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
        thurvtl__subcmd__help__subcmd__cartridge__subcmd__reset__subcmd__stats)
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
        thurvtl__subcmd__help__subcmd__changer)
            opts="inventory move load unload"
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
        thurvtl__subcmd__help__subcmd__changer__subcmd__inventory)
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
        thurvtl__subcmd__help__subcmd__changer__subcmd__load)
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
        thurvtl__subcmd__help__subcmd__changer__subcmd__move)
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
        thurvtl__subcmd__help__subcmd__changer__subcmd__unload)
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
        thurvtl__subcmd__help__subcmd__config)
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
        thurvtl__subcmd__help__subcmd__config__subcmd__completion)
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
        thurvtl__subcmd__help__subcmd__config__subcmd__defaults)
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
        thurvtl__subcmd__help__subcmd__config__subcmd__systemd__subcmd__unit)
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
        thurvtl__subcmd__help__subcmd__drive)
            opts="status self-test reset-stats"
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
        thurvtl__subcmd__help__subcmd__drive__subcmd__reset__subcmd__stats)
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
        thurvtl__subcmd__help__subcmd__drive__subcmd__self__subcmd__test)
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
        thurvtl__subcmd__help__subcmd__drive__subcmd__status)
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
        thurvtl__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__help__subcmd__iscsi)
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
        thurvtl__subcmd__help__subcmd__iscsi__subcmd__target)
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
        thurvtl__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__clear)
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
        thurvtl__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__set)
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
        thurvtl__subcmd__help__subcmd__iscsi__subcmd__target__subcmd__show)
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
        thurvtl__subcmd__help__subcmd__iscsi__subcmd__users)
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
        thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__add)
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
        thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__disable)
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
        thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__enable)
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
        thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__list)
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
        thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__remove)
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
        thurvtl__subcmd__help__subcmd__iscsi__subcmd__users__subcmd__rotate)
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
        thurvtl__subcmd__help__subcmd__library)
            opts="info bounds restore restore-archive monitor self-test partition"
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
        thurvtl__subcmd__help__subcmd__library__subcmd__bounds)
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
        thurvtl__subcmd__help__subcmd__library__subcmd__info)
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
        thurvtl__subcmd__help__subcmd__library__subcmd__monitor)
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
        thurvtl__subcmd__help__subcmd__library__subcmd__partition)
            opts="list create modify delete"
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
        thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__create)
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
        thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__delete)
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
        thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__list)
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
        thurvtl__subcmd__help__subcmd__library__subcmd__partition__subcmd__modify)
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
        thurvtl__subcmd__help__subcmd__library__subcmd__restore)
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
        thurvtl__subcmd__help__subcmd__library__subcmd__restore__subcmd__archive)
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
        thurvtl__subcmd__help__subcmd__library__subcmd__self__subcmd__test)
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
        thurvtl__subcmd__help__subcmd__system)
            opts="gc audit storage stats daemon-health monitor reset-stats verify tiering regenerate-cert alerting"
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
        thurvtl__subcmd__help__subcmd__system__subcmd__alerting)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__alerting__subcmd__list)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__alerting__subcmd__test)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__audit)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__export)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__rotate)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__tail)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__audit__subcmd__verify__subcmd__offline)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__daemon__subcmd__health)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__gc)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__monitor)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__regenerate__subcmd__cert)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__reset__subcmd__stats)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__stats)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__storage)
            opts="check benchmark"
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
        thurvtl__subcmd__help__subcmd__system__subcmd__storage__subcmd__benchmark)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__storage__subcmd__check)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__tiering)
            opts="plan run-now status"
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
        thurvtl__subcmd__help__subcmd__system__subcmd__tiering__subcmd__plan)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__tiering__subcmd__run__subcmd__now)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__tiering__subcmd__status)
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
        thurvtl__subcmd__help__subcmd__system__subcmd__verify)
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
        thurvtl__subcmd__iscsi)
            opts="-c -h --config --user --help users target help"
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
        thurvtl__subcmd__iscsi__subcmd__help)
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
        thurvtl__subcmd__iscsi__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__iscsi__subcmd__help__subcmd__target)
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
        thurvtl__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__clear)
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
        thurvtl__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__set)
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
        thurvtl__subcmd__iscsi__subcmd__help__subcmd__target__subcmd__show)
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
        thurvtl__subcmd__iscsi__subcmd__help__subcmd__users)
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
        thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__add)
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
        thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__disable)
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
        thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__enable)
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
        thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__list)
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
        thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__remove)
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
        thurvtl__subcmd__iscsi__subcmd__help__subcmd__users__subcmd__rotate)
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
        thurvtl__subcmd__iscsi__subcmd__target)
            opts="-c -h --config --user --help show set clear help"
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
        thurvtl__subcmd__iscsi__subcmd__target__subcmd__clear)
            opts="-c -h --config --user --help"
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
        thurvtl__subcmd__iscsi__subcmd__target__subcmd__help)
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
        thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__clear)
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
        thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__set)
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
        thurvtl__subcmd__iscsi__subcmd__target__subcmd__help__subcmd__show)
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
        thurvtl__subcmd__iscsi__subcmd__target__subcmd__set)
            opts="-c -h --username --password --password-stdin --config --user --help"
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
        thurvtl__subcmd__iscsi__subcmd__target__subcmd__show)
            opts="-c -h --json --config --user --help"
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
        thurvtl__subcmd__iscsi__subcmd__users)
            opts="-c -h --config --user --help list add remove disable enable rotate help"
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__add)
            opts="-c -h --password --password-stdin --mutual-chap --partition --config --user --help <NAME>"
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__disable)
            opts="-c -h --config --user --help <NAME>"
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__enable)
            opts="-c -h --config --user --help <NAME>"
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__help)
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__add)
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__disable)
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__enable)
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__list)
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__remove)
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__help__subcmd__rotate)
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__list)
            opts="-c -h --json --config --user --help"
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__remove)
            opts="-c -h --config --user --help <NAME>"
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
        thurvtl__subcmd__iscsi__subcmd__users__subcmd__rotate)
            opts="-c -h --password --password-stdin --grace --cancel --config --user --help <NAME>"
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
        thurvtl__subcmd__library)
            opts="-c -h --config --user --help info bounds restore restore-archive monitor self-test partition help"
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
        thurvtl__subcmd__library__subcmd__bounds)
            opts="-c -h --json --config --user --help"
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
        thurvtl__subcmd__library__subcmd__help)
            opts="info bounds restore restore-archive monitor self-test partition help"
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
        thurvtl__subcmd__library__subcmd__help__subcmd__bounds)
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
        thurvtl__subcmd__library__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__library__subcmd__help__subcmd__info)
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
        thurvtl__subcmd__library__subcmd__help__subcmd__monitor)
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
        thurvtl__subcmd__library__subcmd__help__subcmd__partition)
            opts="list create modify delete"
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
        thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__create)
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
        thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__delete)
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
        thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__list)
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
        thurvtl__subcmd__library__subcmd__help__subcmd__partition__subcmd__modify)
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
        thurvtl__subcmd__library__subcmd__help__subcmd__restore)
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
        thurvtl__subcmd__library__subcmd__help__subcmd__restore__subcmd__archive)
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
        thurvtl__subcmd__library__subcmd__help__subcmd__self__subcmd__test)
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
        thurvtl__subcmd__library__subcmd__info)
            opts="-c -h --json --with-cartridges --config --user --help"
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
        thurvtl__subcmd__library__subcmd__monitor)
            opts="-c -h --interval --config --user --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --interval)
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
        thurvtl__subcmd__library__subcmd__partition)
            opts="-c -h --config --user --help list create modify delete help"
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
        thurvtl__subcmd__library__subcmd__partition__subcmd__create)
            opts="-c -h --storage-start --storage-end --mail-start --mail-end --drives --config --user --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --storage-start)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --storage-end)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mail-start)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mail-end)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --drives)
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
        thurvtl__subcmd__library__subcmd__partition__subcmd__delete)
            opts="-c -h --merge-into --config --user --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --merge-into)
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
        thurvtl__subcmd__library__subcmd__partition__subcmd__help)
            opts="list create modify delete help"
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
        thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__create)
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
        thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__delete)
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
        thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__list)
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
        thurvtl__subcmd__library__subcmd__partition__subcmd__help__subcmd__modify)
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
        thurvtl__subcmd__library__subcmd__partition__subcmd__list)
            opts="-c -h --json --config --user --help"
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
        thurvtl__subcmd__library__subcmd__partition__subcmd__modify)
            opts="-c -h --storage-start --storage-end --mail-start --mail-end --drives --config --user --help <NAME>"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 4 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --storage-start)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --storage-end)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mail-start)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --mail-end)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --drives)
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
        thurvtl__subcmd__library__subcmd__restore)
            opts="-c -h --backend --barcodes --dry-run --allow-existing --config --user --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --backend)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --barcodes)
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
        thurvtl__subcmd__library__subcmd__restore__subcmd__archive)
            opts="-c -h --backend --barcode --label --as-barcode --allow-existing --dry-run --config --user --help"
            if [[ ${cur} == -* || ${COMP_CWORD} -eq 3 ]] ; then
                COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
                return 0
            fi
            case "${prev}" in
                --backend)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --barcode)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --label)
                    COMPREPLY=($(compgen -f "${cur}"))
                    return 0
                    ;;
                --as-barcode)
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
        thurvtl__subcmd__library__subcmd__self__subcmd__test)
            opts="-c -h --json --config --user --help"
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
        thurvtl__subcmd__system)
            opts="-c -h --config --user --help gc audit storage stats daemon-health monitor reset-stats verify tiering regenerate-cert alerting help"
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
        thurvtl__subcmd__system__subcmd__alerting)
            opts="-c -h --config --user --help list test help"
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
        thurvtl__subcmd__system__subcmd__alerting__subcmd__help)
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
        thurvtl__subcmd__system__subcmd__alerting__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__system__subcmd__alerting__subcmd__help__subcmd__list)
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
        thurvtl__subcmd__system__subcmd__alerting__subcmd__help__subcmd__test)
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
        thurvtl__subcmd__system__subcmd__alerting__subcmd__list)
            opts="-c -h --json --config --user --help"
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
        thurvtl__subcmd__system__subcmd__alerting__subcmd__test)
            opts="-c -h --severity --config --user --help <SINK>"
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
        thurvtl__subcmd__system__subcmd__audit)
            opts="-c -h --config --user --help tail export verify verify-offline rotate help"
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
        thurvtl__subcmd__system__subcmd__audit__subcmd__export)
            opts="-c -h --format --from --to --config --user --help"
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
        thurvtl__subcmd__system__subcmd__audit__subcmd__help)
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
        thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__export)
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
        thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__rotate)
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
        thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__tail)
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
        thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify)
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
        thurvtl__subcmd__system__subcmd__audit__subcmd__help__subcmd__verify__subcmd__offline)
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
        thurvtl__subcmd__system__subcmd__audit__subcmd__rotate)
            opts="-c -h --accept-break --config --user --help"
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
        thurvtl__subcmd__system__subcmd__audit__subcmd__tail)
            opts="-f -n -c -h --follow --lines --config --user --help"
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
        thurvtl__subcmd__system__subcmd__audit__subcmd__verify)
            opts="-c -h --config --user --help"
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
        thurvtl__subcmd__system__subcmd__audit__subcmd__verify__subcmd__offline)
            opts="-c -h --dir --json --config --user --help"
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
        thurvtl__subcmd__system__subcmd__daemon__subcmd__health)
            opts="-c -h --json --config --user --help"
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
        thurvtl__subcmd__system__subcmd__gc)
            opts="-c -h --dry-run --storage --config --user --help"
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
        thurvtl__subcmd__system__subcmd__help)
            opts="gc audit storage stats daemon-health monitor reset-stats verify tiering regenerate-cert alerting help"
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
        thurvtl__subcmd__system__subcmd__help__subcmd__alerting)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__alerting__subcmd__list)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__alerting__subcmd__test)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__audit)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__export)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__rotate)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__tail)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__audit__subcmd__verify__subcmd__offline)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__daemon__subcmd__health)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__gc)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__monitor)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__regenerate__subcmd__cert)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__reset__subcmd__stats)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__stats)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__storage)
            opts="check benchmark"
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
        thurvtl__subcmd__system__subcmd__help__subcmd__storage__subcmd__benchmark)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__storage__subcmd__check)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__tiering)
            opts="plan run-now status"
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
        thurvtl__subcmd__system__subcmd__help__subcmd__tiering__subcmd__plan)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__tiering__subcmd__run__subcmd__now)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__tiering__subcmd__status)
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
        thurvtl__subcmd__system__subcmd__help__subcmd__verify)
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
        thurvtl__subcmd__system__subcmd__monitor)
            opts="-c -h --config --user --help"
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
        thurvtl__subcmd__system__subcmd__regenerate__subcmd__cert)
            opts="-c -h --config --user --help"
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
        thurvtl__subcmd__system__subcmd__reset__subcmd__stats)
            opts="-c -h --config --user --help"
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
        thurvtl__subcmd__system__subcmd__stats)
            opts="-c -h --json --config --user --help"
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
        thurvtl__subcmd__system__subcmd__storage)
            opts="-c -h --config --user --help check benchmark help"
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
        thurvtl__subcmd__system__subcmd__storage__subcmd__benchmark)
            opts="-c -h --backend --total-gb --chunk-size-mb --concurrency --chunk-size-mb-sweep --concurrency-sweep --skip-download --yes --config --user --help"
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
        thurvtl__subcmd__system__subcmd__storage__subcmd__check)
            opts="-c -h --config --user --help"
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
        thurvtl__subcmd__system__subcmd__storage__subcmd__help)
            opts="check benchmark help"
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
        thurvtl__subcmd__system__subcmd__storage__subcmd__help__subcmd__benchmark)
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
        thurvtl__subcmd__system__subcmd__storage__subcmd__help__subcmd__check)
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
        thurvtl__subcmd__system__subcmd__storage__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__system__subcmd__tiering)
            opts="-c -h --config --user --help plan run-now status help"
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
        thurvtl__subcmd__system__subcmd__tiering__subcmd__help)
            opts="plan run-now status help"
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
        thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__help)
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
        thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__plan)
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
        thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__run__subcmd__now)
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
        thurvtl__subcmd__system__subcmd__tiering__subcmd__help__subcmd__status)
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
        thurvtl__subcmd__system__subcmd__tiering__subcmd__plan)
            opts="-c -h --json --config --user --help"
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
        thurvtl__subcmd__system__subcmd__tiering__subcmd__run__subcmd__now)
            opts="-c -h --json --config --user --help"
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
        thurvtl__subcmd__system__subcmd__tiering__subcmd__status)
            opts="-c -h --json --config --user --help"
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
        thurvtl__subcmd__system__subcmd__verify)
            opts="-c -h --skip-storage --verbose --json --config --user --help [BARCODES]..."
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
    esac
}

if [[ "${BASH_VERSINFO[0]}" -eq 4 && "${BASH_VERSINFO[1]}" -ge 4 || "${BASH_VERSINFO[0]}" -gt 4 ]]; then
    complete -F _thurvtl -o nosort -o bashdefault -o default thurvtl
else
    complete -F _thurvtl -o bashdefault -o default thurvtl
fi
