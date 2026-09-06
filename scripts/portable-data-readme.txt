chiaki-remaster (portable Version)
==================================

Dieser "data"-Ordner macht diese Installation portabel: Es werden keine
Daten ins Benutzerprofil (AppData) geschrieben, alles bleibt im
Programmverzeichnis neben der chiaki.exe.

Hier landen:
- settings.ini              alle Einstellungen
- profiles/*.ini            Verbindungsprofile inkl. PSN-Daten
                            (Refresh-Token, Account-IDs)
- placebo_render_params.ini Render-Parameter (libplacebo)
- log/                      Logdateien
- cache/                    Cache-Dateien

Diesen Ordner bei Updates oder Umzügen mitkopieren (oder das ganze
Programmverzeichnis sichern), sonst gehen Einstellungen und
PSN-Anmeldung verloren.
