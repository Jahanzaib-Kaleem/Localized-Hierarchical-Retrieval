"""Deterministic hierarchy portfolio selection.

No semantics or learned model: choose projections from observed cardinality and a disk budget.
"""
from __future__ import annotations
from itertools import combinations
from math import prod
from .streaming import HierarchySpec

def choose_hierarchies(cardinalities, *, max_width=3, max_hierarchies=32, max_keyspace=1_000_000):
    candidates=[]
    for width in range(2,max_width+1):
        for cols in combinations(range(len(cardinalities)),width):
            space=prod(cardinalities[c] for c in cols)
            if space<=max_keyspace:
                # Higher theoretical selectivity per column, then smaller keyspace.
                score=space/width
                candidates.append((score,space,cols))
    candidates.sort(key=lambda x:(-x[0],x[1],x[2]))
    return [HierarchySpec(tuple(x[2])) for x in candidates[:max_hierarchies]]
