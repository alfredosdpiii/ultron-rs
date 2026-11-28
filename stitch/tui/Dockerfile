ARG STITCH_BASE
FROM ${STITCH_BASE}
COPY --from=source . /src
RUN for f in /src/crates/theme/src/lib.rs /src/crates/tui/src/lib.rs; do \
      sed -i '1i #![feature(result_option_map_or_default)]' "$f"; \
    done
