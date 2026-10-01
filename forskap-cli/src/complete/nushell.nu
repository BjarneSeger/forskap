# Dynamic completion for forskap: every Tab asks `forskap` itself.
def "nu-complete forskap" [place: any] {
    # Nushell 0.116+ hands over a record, 0.108 to 0.115 the words themselves.
    let words = if ($place | describe) starts-with "record" { $place.command } else { $place }
    let found = try {
        with-env { COMPLETE: nushell } { ^r#'{completer}'# -- ...$words } | complete | get stdout | from json
    } catch { null }
    # A plain list: 0.108 takes no record with options here, and neither sorts it.
    $found | default []
}

@complete "nu-complete forskap"
extern forskap [...args: string]
