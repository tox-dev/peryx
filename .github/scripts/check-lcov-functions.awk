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

function function_name(value,    rest, separator, end_line) {
    rest = tail(value)
    separator = index(rest, ",")
    if (!separator) {
        return rest
    }
    end_line = substr(rest, 1, separator - 1)
    return end_line ~ /^[0-9]+$/ ? substr(rest, separator + 1) : rest
}

/^SF:/ {
    source = substr($0, 4)
    local = index(source, root) == 1
}

local && /^FN:/ {
    value = substr($0, 4)
    function_key = source SUBSEP function_name(value)
    declaration = source SUBSEP field(value)
    function_declaration[function_key] = declaration
    source_function[declaration] = 1
}

local && /^FNDA:/ {
    value = substr($0, 6)
    function_hits[source SUBSEP tail(value)] += field(value)
}

local && /^FNL:/ {
    value = substr($0, 5)
    function_key = source SUBSEP field(value)
    declaration = source SUBSEP tail(value)
    function_declaration[function_key] = declaration
    source_function[declaration] = 1
}

local && /^FNA:/ {
    value = substr($0, 5)
    rest = tail(value)
    function_hits[source SUBSEP field(value)] += field(rest)
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
