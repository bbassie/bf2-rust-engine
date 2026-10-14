bf2-rust-engine {VERSION}
=========================

A from-scratch Battlefield 2 style shooter: conquest and co-op on BF2's maps, vehicles,
bots, singleplayer and multiplayer. It needs your own copy of Battlefield 2 (1.5, with Special
Forces and the booster packs if you have them): the game converts BF2's maps, vehicles,
weapons and sounds from your installation once, on your computer. No BF2 files come with this
download and nothing is uploaded.

Starting
--------

Windows: run client.exe.

The first start opens a welcome page. It looks for Battlefield 2 (the EA and BF2Hub registry
entries and the usual folders on every drive); if it doesn't find yours, type the folder that
has BF2.exe and a "mods" folder in it and press "Use this folder". Pick any BF2 mods you also
want (AIX 2, for example: it becomes a mod of this game with its own levels), then Import.
Importing everything takes about 10-15 minutes and 5 GB of disk space ("Quick start" imports
two levels in about a minute; import the rest later). You can cancel at any time: the next
import continues where it stopped. Settings > Game > BF2 content imports more later.

Where things go
---------------

  Imported content and mods  %LOCALAPPDATA%\bf2-rust-engine\imported and ...\mods
                             (Linux: ~/.local/share/bf2-rust-engine)
  Import log                 %LOCALAPPDATA%\bf2-rust-engine\import.log
  Settings, account, server  %APPDATA%\bf2-rust-engine
  Caches (bots' navigation,  %LOCALAPPDATA%\bf2-rust-engine\cache
  maps; safe to delete)

client.exe --data-dir <folder> (and server.exe / bf2-import.exe --data-dir) keeps imported
content and mods somewhere else. A folder named "imported" next to client.exe is used instead
of the one above, for a game folder you carry around with its content. Delete the folders
above to uninstall; the game itself is just this folder.

Playing together
----------------

No accounts are needed. On a LAN:

  Host: Main menu > Host, switch on "Public" so other machines can join, pick a level, Start.
  Join: Main menu > Join lists servers on your network; or type the host's address.

Everyone needs the base content imported. A host can share content its players lack
(mods, or everything, if its admin allows it): joining asks before downloading.

Over the internet the host forwards UDP and TCP port 16567 (the game, and content downloads)
to its computer and tells friends its address. LAN discovery uses UDP ports 16568-16575.
A master server (an internet server list with optional accounts and stats) is optional: run
your own and set its address in Settings > Account, or ignore it.

Dedicated server
----------------

  server.exe --level strike_at_karkand --bots 16 --public
  server.exe --help                  all options (map rotation, admin password, RCON, ...)

It uses the same imported content as the game (or --data-dir). Linux servers: the
bf2-rust-engine-{VERSION}-linux-server package has server and bf2-import; import there with

  ./bf2-import --bf2 "/path/to/Battlefield 2" level --base-game

Default controls
----------------

WASD move, Shift sprint, Space jump, Ctrl crouch, Z prone, mouse fire and zoom, R reload,
1-9 or the wheel switch weapons, E enter/leave vehicles (F1-F8 switch seats), V third person,
Q commo rose, M map, Caps Lock commander screen, Enter deploy screen, Tab scoreboard, T/Y/U
chat, Esc menu. Everything is rebindable in Settings > Controls; gamepads work too.

Legal
-----

Battlefield 2 is a trademark of Electronic Arts. This project is not affiliated with or
endorsed by EA or DICE and contains no EA code or assets; the importer only reads files from
an installation you own, on your own machine. Content it converts is for your personal use:
don't redistribute it. This program is licensed under MIT OR Apache-2.0; third-party
components and their licenses are listed in THIRD-PARTY-NOTICES.txt.
