#!/bin/sh
# Verifier: exit 0 = pass. Output is the feedback gradient.
python3 -c "import sys; sys.path.insert(0,'.'); from solution import add; assert add(2,3)==5 and add(-1,1)==0; print('PASS')"
