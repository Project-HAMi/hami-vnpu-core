#!/bin/bash

gcc -I${ASCEND_TOOLKIT_HOME}/include -L${ASCEND_TOOLKIT_HOME}/lib64 -o memory_test_loop ./test_memory_loop.c -lascendcl -lacl_op_compiler
