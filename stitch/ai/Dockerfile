ARG STITCH_BASE
FROM ${STITCH_BASE}
COPY --from=source . /src
RUN for f in /src/crates/*/src/lib.rs; do \
      sed -i '1i #![feature(result_option_map_or_default, round_char_boundary, duration_constructors_lite)]' "$f"; \
    done
