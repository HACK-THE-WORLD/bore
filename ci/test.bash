#!/usr/bin/env bash
# Script for building your rust projects.
set -e

source ci/common.bash

# $1 {path} = Path to cross/cargo executable
CROSS=$1
# $1 {string} = <Target Triple>
TARGET_TRIPLE=$2

required_arg $CROSS 'CROSS'
required_arg $TARGET_TRIPLE '<Target Triple>'

max_attempts=3
count=0
status=1

while [ $count -lt $max_attempts ]; do
    if $CROSS test --target $TARGET_TRIPLE; then
        echo "Test passed"
        status=0
        break
    else
        status=$?
        echo "Test failed, attempt $(($count + 1))"
    fi
    count=$(($count + 1))
done

if [ $status -ne 0 ]; then
    echo "Test failed after $max_attempts attempts"
fi

exit $status
