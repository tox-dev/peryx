#!/usr/bin/awk -f

BEGIN {
    root = ENVIRON["COVERAGE_ROOT"]
    if (!root) {
        root = ENVIRON["PWD"]
    }
    if (substr(root, length(root)) != "/") {
        root = root "/"
    }
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

function normalized_name(name,    character, depth, position, normalized) {
    sub(/[.]llvm[.][[:xdigit:]]+$/, "", name)
    sub(/[.](constprop|isra|part)[.][0-9]+$/, "", name)
    sub(/::h[[:xdigit:]]+$/, "", name)
    for (position = 1; position <= length(name); position++) {
        character = substr(name, position, 1)
        if (character == "<") {
            depth++
        } else if (character == ">") {
            depth--
        } else if (!depth) {
            normalized = normalized character
        }
    }
    sub(/::$/, "", normalized)
    return normalized
}

function load_declarations(path,    brace, declaration_key, line, line_number, name, rest, semicolon, token) {
    if (path in declarations_loaded) {
        return
    }
    declarations_loaded[path] = 1
    while ((getline line < path) > 0) {
        line_number++
        rest = line
        sub(/\/\/.*/, "", rest)
        while (length(rest)) {
            if (!length(name)) {
                if (!match(rest, /(^|[^[:alnum:]_])fn[[:space:]]+[[:alpha:]_][[:alnum:]_]*([[:space:]<(]|$)/)) {
                    break
                }
                token = substr(rest, RSTART, RLENGTH)
                sub(/^.*fn[[:space:]]+/, "", token)
                sub(/[[:space:]<(].*$/, "", token)
                name = token
                pending_line = line_number
                rest = substr(rest, RSTART + RLENGTH)
            }
            brace = index(rest, "{")
            semicolon = index(rest, ";")
            if (!brace && !semicolon) {
                break
            }
            if (semicolon && (!brace || semicolon < brace)) {
                name = ""
                rest = substr(rest, semicolon + 1)
                continue
            }
            declaration_key = path SUBSEP pending_line SUBSEP name
            source_declaration[declaration_key] = 1
            declaration_at_line[path SUBSEP pending_line] = 1
            declaration_count[path]++
            declaration_key_at[path SUBSEP declaration_count[path]] = declaration_key
            declaration_name_at[path SUBSEP declaration_count[path]] = name
            declaration_line_at[path SUBSEP declaration_count[path]] = pending_line
            name = ""
            rest = substr(rest, brace + 1)
        }
    }
    close(path)
}

function resolve_identity(symbol, path, generated_line,    candidate, candidate_distance, candidate_line, candidate_name,
        distance, name_matches, position, symbol_name) {
    load_declarations(path)
    resolved_identity = ""
    resolved_line = generated_line
    symbol_name = normalized_name(symbol)
    for (position = 1; position <= declaration_count[path]; position++) {
        candidate_name = declaration_name_at[path SUBSEP position]
        candidate_line = declaration_line_at[path SUBSEP position] + 0
        if (candidate_line == generated_line) {
            resolved_identity = candidate_name
            resolved_line = candidate_line
            return
        }
        name_matches = symbol_name == candidate_name || index(symbol, length(candidate_name) candidate_name) > 0
        distance = generated_line - candidate_line
        if (distance < 0) {
            distance = -distance + 1000000
        }
        if (!name_matches) {
            distance += 2000000
        }
        if (!length(candidate) || distance < candidate_distance) {
            candidate = candidate_name
            candidate_distance = distance
            resolved_line = candidate_line
        }
    }
    if (length(candidate)) {
        resolved_identity = candidate
        return
    }
    resolved_identity = symbol_name
    resolved_line = generated_line
}

/^SF:/ {
    source = substr($0, 4)
    local = index(source, root) == 1
    record++
    if (local) {
        load_declarations(source)
    }
}

local && /^DA:/ {
    executable_line[source SUBSEP field(substr($0, 4))] = 1
}

local && /^FN:/ {
    value = substr($0, 4)
    function_key = source SUBSEP record SUBSEP function_name(value)
    function_source[function_key] = source
    function_label[function_key] = function_name(value)
    resolve_identity(function_label[function_key], source, field(value))
    function_line[function_key] = resolved_line
    function_identity[function_key] = resolved_identity
}

local && /^FNDA:/ {
    value = substr($0, 6)
    function_hits[source SUBSEP record SUBSEP tail(value)] += field(value)
}

local && /^FNL:/ {
    value = substr($0, 5)
    function_key = source SUBSEP record SUBSEP field(value)
    function_source[function_key] = source
    function_line[function_key] = tail(value)
}

local && /^FNA:/ {
    value = substr($0, 5)
    rest = tail(value)
    function_key = source SUBSEP record SUBSEP field(value)
    function_hits[function_key] += field(rest)
    function_label[function_key] = tail(rest)
    resolve_identity(function_label[function_key], source, function_line[function_key])
    function_line[function_key] = resolved_line
    function_identity[function_key] = resolved_identity
}

END {
    for (key in function_line) {
        identity = function_identity[key]
        if (!length(identity)) {
            identity = normalized_name(function_label[key])
        }
        declaration = function_source[key] SUBSEP function_line[key] SUBSEP identity
        expected_function[declaration] = 1
        declaration_source[declaration] = function_source[key]
        declaration_line[declaration] = function_line[key]
        declaration_name[declaration] = identity
        declaration_hits[declaration] += function_hits[key]
    }
    for (key in executable_line) {
        split(key, parts, SUBSEP)
        candidate_line = parts[2] + 0
        while (candidate_line > 0 && !((parts[1] SUBSEP candidate_line) in declaration_at_line)) {
            candidate_line--
        }
        if (!candidate_line) {
            continue
        }
        for (position = 1; position <= declaration_count[parts[1]]; position++) {
            if (declaration_line_at[parts[1] SUBSEP position] == candidate_line) {
                declaration = declaration_key_at[parts[1] SUBSEP position]
                expected_function[declaration] = 1
                declaration_source[declaration] = parts[1]
                declaration_line[declaration] = candidate_line
                declaration_name[declaration] = declaration_name_at[parts[1] SUBSEP position]
            }
        }
    }
    for (key in expected_function) {
        total++
        if (declaration_hits[key] > 0) {
            covered++
            continue
        }
        print declaration_source[key] ":" declaration_line[key] ": function " declaration_name[key] " is not covered"
        failed++
    }
    if (ENVIRON["COVERAGE_REQUIRE_EXECUTABLE"] && !total) {
        print root " has no instrumented source functions"
        exit 1
    }
    if (failed) {
        print failed " of " total " source functions are not covered"
    } else {
        print covered + 0 " source functions covered"
    }
    exit failed > 0
}
