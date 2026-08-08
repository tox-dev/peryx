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
    function_line[source SUBSEP function_name(value)] = field(value)
}

local && /^FNDA:/ {
    value = substr($0, 6)
    function_hits[source SUBSEP tail(value)] += field(value)
}

local && /^FNL:/ {
    value = substr($0, 5)
    function_line[source SUBSEP field(value)] = tail(value)
}

local && /^FNA:/ {
    value = substr($0, 5)
    rest = tail(value)
    function_hits[source SUBSEP field(value)] += field(rest)
}

local && /^DA:/ {
    value = substr($0, 4)
    line_hits[source SUBSEP field(value)] += tail(value)
}

END {
    for (key in function_hits) {
        if (function_hits[key] > 0 && key in function_line) {
            split(key, parts, SUBSEP)
            covered_function_line[parts[1] SUBSEP function_line[key]] = 1
        }
    }
    for (key in line_hits) {
        if (line_hits[key] > 0 || key in covered_function_line) {
            continue
        }
        split(key, parts, SUBSEP)
        print parts[1] ":" parts[2]
        failed = 1
    }
    exit failed
}
