# 按 Desktop Entry 的参数规则解析 Exec，并将每个参数转成 shell 字面量。
entry=$xsessions/$desktop_id.desktop
[ -f "$entry" ] || fail desktop_missing
parsed=$(awk '
function q(s,   i,c,r) {r="\047";for(i=1;i<=length(s);i++){c=substr(s,i,1);r=r (c=="\047"?"\047\\\047\047":c)}return r "\047"}
/^\[/ {active=($0=="[Desktop Entry]")}
active && /^Name=/ {name=substr($0,6)}
active && /^DesktopNames=/ {names=substr($0,14);gsub(/;/,":",names);sub(/:$/, "", names)}
active && /^(Hidden|NoDisplay)=true\r?$/ {hidden=1}
active && /^Exec=/ {exec=substr($0,6)}
END {
    if(hidden || exec=="" || name=="") exit 1
    quoted=0; escaped=0; token=""; present=0; count=0
    for(i=1;i<=length(exec)+1;i++) {
        c=substr(exec,i,1)
        if(escaped) {token=token c;escaped=0;present=1}
        else if(c=="\\") escaped=1
        else if(c=="\"") {quoted=!quoted;present=1}
        else if(!quoted && (c ~ /[ \t]/ || c=="")) {
            if(present) {
                if(token ~ /%[fFuUdDnNickvVm]/) {if(token !~ /^%[fFuUdDnNickvVm]$/) exit 1}
                else {gsub(/%%/,"%",token);args=args " " q(token);count++;if(count==1) first=token}
            }
            token="";present=0
        } else {token=token c;present=1}
    }
    if(quoted || escaped || count==0) exit 1
    printf "desktop_name=%s\ndesktop_names=%s\ndesktop_exec=%s\nexecutable=%s\n",q(name),q(names),q(args),q(first)
}' "$entry") || fail desktop_missing
[ -n "$parsed" ] || fail desktop_missing
eval "$parsed"
command -v "$executable" >/dev/null 2>&1 || fail desktop_missing
