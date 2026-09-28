# Reference oracle for the LSL conformance suite.
#
# This image builds one exact revision of liblsl. The conformance vectors come
# from this image and from no other build. If a vector changes, either a pin in
# oracle/PINS changed, or a defect exists.
#
# Build from the repository root, not from this directory:
#   docker build -f oracle/Dockerfile -t lsl-oracle .

# The digest pins the compiler, cmake 3.28.3, and Catch2 3.4.0 together.
FROM ubuntu@sha256:4fbb8e6a8395de5a7550b33509421a2bafbc0aab6c06ba2cef9ebffbc7092d90

# catch2 3.4.0 in this image matches the version that liblsl fetches. Catch2
# runs the tests and never changes the bytes that liblsl sends, so the packaged
# build is safe. liblsl finds it through find_package and skips the fetch.
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential \
        cmake \
        catch2 \
        git \
        ca-certificates \
        python3 \
    && rm -rf /var/lib/apt/lists/*

# pugixml evaluates the XPath query that decides whether a stream matches. Its
# version changes oracle behavior, so pin the commit and not the tag.
ARG PUGIXML_COMMIT=ee86beb30e4973f5feffe3ce63bfa4fbadf72f38

RUN git clone --filter=blob:none https://github.com/zeux/pugixml.git /deps/pugixml \
    && git -C /deps/pugixml checkout --detach ${PUGIXML_COMMIT} \
    && rm -rf /deps/pugixml/.git

# The source tree is copied, not cloned. The caller controls the revision.
COPY liblsl /oracle/src
COPY oracle/PINS /oracle/PINS

# Confirm that the copied tree is the pinned revision. A wrong revision here
# produces wrong vectors, and a wrong vector is worse than no vector.
RUN EXPECTED=$(grep '^LIBLSL_COMMIT=' /oracle/PINS | cut -d= -f2) \
    && ACTUAL=$(git -C /oracle/src rev-parse HEAD) \
    && if [ "$EXPECTED" != "$ACTUAL" ]; then \
         echo "ORACLE PIN MISMATCH"; \
         echo "  expected $EXPECTED"; \
         echo "  actual   $ACTUAL"; \
         exit 1; \
       fi \
    && echo "oracle revision confirmed: $ACTUAL"

# SOURCE_DATE_EPOCH makes timestamps in build output stable.
ENV SOURCE_DATE_EPOCH=1

RUN cmake -S /oracle/src -B /oracle/build \
        -DCMAKE_BUILD_TYPE=Release \
        -DLSL_UNITTESTS=ON \
        -DLSL_BENCHMARKS=OFF \
        -DLSL_TESTS_PREFER_SYSTEM_CATCH2=ON \
        -DFETCHCONTENT_SOURCE_DIR_PUGIXML=/deps/pugixml \
    && cmake --build /oracle/build -j"$(nproc)"

COPY oracle/entrypoint.sh /oracle/entrypoint.sh
RUN chmod +x /oracle/entrypoint.sh

WORKDIR /oracle
ENTRYPOINT ["/oracle/entrypoint.sh"]
CMD ["info"]
