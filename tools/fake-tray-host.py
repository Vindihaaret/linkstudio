#!/usr/bin/env python3
"""Development aid: a minimal StatusNotifierWatcher, so Link Studio shows its
status icon inside an isolated `dbus-run-session` (see CLAUDE.md). The item
name it registers is written to the file given as the first argument."""
import gi,sys; gi.require_version("Gio","2.0")
from gi.repository import Gio, GLib
XML="""<node><interface name="org.kde.StatusNotifierWatcher">
<method name="RegisterStatusNotifierItem"><arg type="s" direction="in"/></method>
<method name="RegisterStatusNotifierHost"><arg type="s" direction="in"/></method>
<property name="RegisteredStatusNotifierItems" type="as" access="read"/>
<property name="IsStatusNotifierHostRegistered" type="b" access="read"/>
<property name="ProtocolVersion" type="i" access="read"/>
<signal name="StatusNotifierItemRegistered"><arg type="s"/></signal>
<signal name="StatusNotifierHostRegistered"/></interface></node>"""
def call(c,s,p,i,m,params,inv):
    if m=="RegisterStatusNotifierItem": open(sys.argv[1],"w").write(params.unpack()[0])
    inv.return_value(None)
def get(c,s,p,i,prop):
    return {"RegisteredStatusNotifierItems":GLib.Variant("as",[]),"IsStatusNotifierHostRegistered":GLib.Variant("b",True),"ProtocolVersion":GLib.Variant("i",0)}[prop]
def acquired(c,n): c.register_object("/StatusNotifierWatcher",Gio.DBusNodeInfo.new_for_xml(XML).interfaces[0],call,get,None)
Gio.bus_own_name(Gio.BusType.SESSION,"org.kde.StatusNotifierWatcher",0,acquired,None,None)
GLib.MainLoop().run()
