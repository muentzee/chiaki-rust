chiaki-remaster (portable version)
==================================

This "data" folder makes the installation portable: nothing is written to
the user profile (AppData), everything stays in the program directory
next to chiaki.exe.

This folder contains:
- settings.ini              all settings
- profiles/*.ini            connection profiles incl. PSN data
                            (refresh tokens, account IDs)
- placebo_render_params.ini render parameters (libplacebo)
- log/                      log files
- cache/                    cache files

Copy this folder when updating or moving the installation (or back up the
whole program directory), otherwise your settings and PSN sign-in are lost.
