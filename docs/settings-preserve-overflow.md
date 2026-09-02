# `a_full_set_of_bad_slots_still_moves_the_corrupt_file_out_of_harms_way`

Running out of `.bad` slots must not leave the corrupt file where the
next `save()` will write over it.

The whole point of moving it aside is that `load()` runs at startup and
the next `save()` — opening Settings, a window resize — writes defaults
straight onto that path. With every numbered slot taken, preservation
gave up and left the file exactly there, so the copy still holding the
user's keyserver token, output folder and keydb path was destroyed by
the first save of the session. Losing the tenth-oldest preserved copy
is the only alternative, and it is the cheaper one.
