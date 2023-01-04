package metadata

base: components: transforms: remap: configuration: {
	drop_on_abort: {
		description: """
			Drops any event that is manually aborted during processing.

			Normally, if a VRL program is manually aborted (via [`abort`][vrl_docs_abort]) when
			processing an event, the original, unmodified event will be sent downstream. In some cases,
			you may not wish to send the event any further, such as if certain transformation or
			enrichment is strictly required. Setting `drop_on_abort` to `true` allows you to ensure
			these events do not get processed any further.

			Additionally, dropped events can potentially be diverted to a specially-named output for
			further logging and analysis by setting `reroute_dropped`.

			[vrl_docs_abort]: https://vector.dev/docs/reference/vrl/expressions/#abort
			"""
		required: false
		type: bool: default: true
	}
	drop_on_error: {
		description: """
			Drops any event that encounters an error during processing.

			Normally, if a VRL program encounters an error when processing an event, the original,
			unmodified event will be sent downstream. In some cases, you may not wish to send the event
			any further, such as if certain transformation or enrichment is strictly required. Setting
			`drop_on_error` to `true` allows you to ensure these events do not get processed any
			further.

			Additionally, dropped events can potentially be diverted to a specially named output for
			further logging and analysis by setting `reroute_dropped`.
			"""
		required: false
		type: bool: default: false
	}
	file: {
		description: """
			File path to the [Vector Remap Language][vrl] (VRL) program to execute for each event.

			If a relative path is provided, its root is the current working directory.

			Required if `source` is missing.

			[vrl]: https://vector.dev/docs/reference/vrl
			"""
		required: false
		type: string: examples: ["./my/program.vrl"]
	}
	metric_tag_values: {
		description: """
			When set to `single`, metric tag values will be exposed as single strings, the
			same as they were before this config option. Tags with multiple values will show the last assigned value, and null values
			will be ignored.

			When set to `full`, all metric tags will be exposed as arrays of either string or null
			values.
			"""
		required: false
		type: string: {
			default: "single"
			enum: {
				full: "All tags will be exposed as arrays of either string or null values."
				single: """
					Tag values will be exposed as single strings, the
					same as they were before this config option. Tags with multiple values will show the last assigned value, and null values
					will be ignored.
					"""
			}
		}
	}
	reroute_dropped: {
		description: """
			Reroutes dropped events to a named output instead of halting processing on them.

			When using `drop_on_error` or `drop_on_abort`, events that are "dropped" are processed no
			further. In some cases, it may be desirable to keep the events around for further analysis,
			debugging, or retrying.

			In these cases, `reroute_dropped` can be set to `true` which will forward the original event
			to a specially-named output, `dropped`. The original event will be annotated with additional
			fields describing why the event was dropped.
			"""
		required: false
		type: bool: default: false
	}
	runtime: {
		description: "Available VRL runtimes."
		required:    false
		type: string: {
			default: "ast"
			enum: ast: """
				Tree-walking runtime.

				This is the only, and default, runtime.
				"""
		}
	}
	source: {
		description: """
			The [Vector Remap Language][vrl] (VRL) program to execute for each event.

			Required if `file` is missing.

			[vrl]: https://vector.dev/docs/reference/vrl
			"""
		required: false
		type: string: {
			examples: ["""
				. = parse_json!(.message)
				.new_field = "new value"
				.status = to_int!(.status)
				.duration = parse_duration!(.duration, "s")
				.new_name = del(.old_name)
				"""]
			syntax: "remap_program"
		}
	}
	timezone: {
		description: """
			The name of the timezone to apply to timestamp conversions that do not contain an explicit
			time zone.

			This overrides the [global `timezone`][global_timezone] option. The time zone name may be
			any name in the [TZ database][tz_database], or `local` to indicate system local time.

			[global_timezone]: https://vector.dev/docs/reference/configuration//global-options#timezone
			[tz_database]: https://en.wikipedia.org/wiki/List_of_tz_database_time_zones
			"""
		required: false
		type: string: examples: ["local", "America/New_York", "EST5EDT"]
	}
}
