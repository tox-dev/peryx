#!/usr/bin/awk -f

BEGIN {
    root = ENVIRON["PWD"] "/"
}

function field(value,    separator) {
    separator = index(value, ",")
    return substr(value, 1, separator - 1)
}

function tail(value,    separator) {
    separator = index(value, ",")
    return substr(value, separator + 1)
}

/^SF:/ {
    source = substr($0, 4)
    local = index(source, root) == 1
}

local && /^FN:/ {
    value = substr($0, 4)
    function_key = source SUBSEP tail(value)
    declaration = source SUBSEP field(value)
    function_declaration[function_key] = declaration
    source_function[declaration] = 1
}

local && /^FNDA:/ {
    value = substr($0, 6)
    function_hits[source SUBSEP tail(value)] += field(value)
}

END {
    for (key in function_hits) {
        if (key in function_declaration) {
            declaration_hits[function_declaration[key]] += function_hits[key]
        }
    }
    for (key in source_function) {
        total++
        if (declaration_hits[key] > 0) {
            covered++
            continue
        }
        split(key, parts, SUBSEP)
        print parts[1] ":" parts[2] ": function is not covered"
        failed++
    }
    if (failed) {
        print failed " of " total " source functions are not covered"
    } else {
        print covered + 0 " source functions covered"
    }
    exit failed > 0
}
