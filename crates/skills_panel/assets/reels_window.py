import os
import sys
import threading

PROFILE_DIRECTORY = sys.argv[1]
PARENT_WINDOW = int(sys.argv[2]) if len(sys.argv) > 2 else None

if PARENT_WINDOW is not None:
    os.environ["GDK_BACKEND"] = "x11"

import gi

gi.require_version("Gdk", "3.0")
gi.require_version("Gtk", "3.0")
gi.require_version("WebKit2", "4.1")
if PARENT_WINDOW is not None:
    gi.require_version("GdkX11", "3.0")
    from gi.repository import GdkX11
from gi.repository import Gdk, GLib, Gtk, WebKit2

REELS_URL = "https://www.instagram.com/reels/"
REVERT_TO_PARENT = 2


def build_web_view():
    data_manager = WebKit2.WebsiteDataManager(
        base_data_directory=f"{PROFILE_DIRECTORY}/data",
        base_cache_directory=f"{PROFILE_DIRECTORY}/cache",
    )
    context = WebKit2.WebContext.new_with_website_data_manager(data_manager)
    context.get_cookie_manager().set_persistent_storage(
        f"{PROFILE_DIRECTORY}/cookies.sqlite",
        WebKit2.CookiePersistentStorage.SQLITE,
    )
    web_view = WebKit2.WebView.new_with_context(context)
    web_view.get_settings().set_media_playback_requires_user_gesture(False)
    web_view.load_uri(REELS_URL)
    return web_view


def run_standalone():
    window = Gtk.Window(title="Instagram Reels")
    window.set_default_size(480, 900)
    window.add(build_web_view())
    window.connect("destroy", Gtk.main_quit)
    window.show_all()
    Gtk.main()


def run_embedded():
    # Reparenting a WebKit window into Zed's window leaves it blank, so the
    # view is a borderless window kept positioned over the panel. It must be a
    # managed window, or the window manager never gives it keyboard focus.
    gdk_display = Gdk.Display.get_default()
    parent_window = GdkX11.X11Window.foreign_new_for_display(gdk_display, PARENT_WINDOW)

    window = Gtk.Window(type=Gtk.WindowType.TOPLEVEL)
    window.set_decorated(False)
    window.set_skip_taskbar_hint(True)
    window.set_skip_pager_hint(True)
    window.set_accept_focus(True)
    window.set_focus_on_map(False)
    web_view = build_web_view()
    window.add(web_view)
    window.realize()
    gdk_window = window.get_window()
    gdk_window.set_transient_for(parent_window)
    own_window = gdk_window.get_xid()

    def focus_on_click(*_):
        gdk_window.focus(Gtk.get_current_event_time())
        return False

    web_view.connect("button-press-event", focus_on_click)

    state = {"visible": False, "x": 0, "y": 0, "width": 1, "height": 1, "applied": None}

    def zed_or_reels_is_active():
        active = Gdk.Screen.get_default().get_active_window()
        return active is not None and active.get_xid() in (PARENT_WINDOW, own_window)

    def update_placement():
        if not (state["visible"] and zed_or_reels_is_active()):
            if window.get_visible():
                window.hide()
            return True
        origin = parent_window.get_origin()
        placement = (
            origin[-2] + state["x"], origin[-1] + state["y"], state["width"], state["height"],
        )
        if placement != state["applied"]:
            state["applied"] = placement
            window.resize(placement[2], placement[3])
            window.move(placement[0], placement[1])
        if not window.get_visible():
            window.show_all()
            window.move(placement[0], placement[1])
        return True

    def press_key(keyval):
        # Instagram's reels viewer moves between reels on the arrow keys. The
        # event is fed to the view directly because Zed keeps keyboard focus.
        if not window.get_visible():
            return
        web_view.grab_focus()
        keymap = Gdk.Keymap.get_for_display(gdk_display)
        found, keys = keymap.get_entries_for_keyval(keyval)
        keyboard = gdk_display.get_default_seat().get_keyboard()
        for event_type in (Gdk.EventType.KEY_PRESS, Gdk.EventType.KEY_RELEASE):
            event = Gdk.Event.new(event_type)
            event.key.window = gdk_window
            event.key.send_event = 1
            event.key.time = Gdk.CURRENT_TIME
            event.key.keyval = keyval
            if found and keys:
                event.key.hardware_keycode = keys[0].keycode
            event.set_device(keyboard)
            web_view.event(event)

    def apply_command(line):
        parts = line.split()
        if not parts:
            return False
        if parts[0] == "bounds" and len(parts) == 5:
            x, y, width, height = (int(value) for value in parts[1:])
            state.update(x=x, y=y, width=max(width, 1), height=max(height, 1))
        elif parts[0] == "show":
            state["visible"] = True
        elif parts[0] == "hide":
            state["visible"] = False
        elif parts[0] == "navigate" and len(parts) == 2:
            press_key(Gdk.KEY_Down if parts[1] == "next" else Gdk.KEY_Up)
        elif parts[0] == "quit":
            Gtk.main_quit()
        update_placement()
        return False

    def read_commands():
        for line in sys.stdin:
            GLib.idle_add(apply_command, line)
        GLib.idle_add(Gtk.main_quit)

    threading.Thread(target=read_commands, daemon=True).start()
    GLib.timeout_add(60, update_placement)
    Gtk.main()


if PARENT_WINDOW is None:
    run_standalone()
else:
    run_embedded()
