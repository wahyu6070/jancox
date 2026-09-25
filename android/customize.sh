
print(){
	echo "$1"
	}
getp(){ grep "^$1" "$2" | head -n1 | cut -d = -f 2; }

MODULEVERSION=`getp version $MODPATH/module.prop`
MODULENAME=`getp name $MODPATH/module.prop`
MODULEDATE=`getp date $MODPATH/module.prop`
MODULEAUTHOR=`getp author $MODPATH/module.prop`

print "____________________________________"
print "|"
print "| Name            : $MODULENAME"
print "| Version         : $MODULEVERSION"
print "| Build date      : $MODULEDATE"
print "| By              : $MODULEAUTHOR"
print "|___________________________________"
print "|"
print "| Telegram Group  : https://t.me/wahyu6070group"
print "|___________________________________"

# ARCH is set by Magisk, KernelSU, APatch and the Kopi installer:
# arm, arm64, x86 or x64
case "$ARCH" in
	arm64) JANCOX_ARCH=arm64 ;;
	arm) JANCOX_ARCH=arm ;;
	x64) JANCOX_ARCH=x86_64 ;;
	x86) JANCOX_ARCH=x86 ;;
	*) abort "[ERROR] Architecture not supported <$ARCH>" ;;
esac

# build.sh puts one jancox binary per architecture in bin/<arch>/
JANCOX_BIN=$MODPATH/bin/$JANCOX_ARCH/jancox
[ -f "$JANCOX_BIN" ] || abort "[ERROR] jancox binary not found <bin/$JANCOX_ARCH/jancox>"

print "- Installing jancox ($JANCOX_ARCH) to /system/bin"
mkdir -p $MODPATH/system/bin
cp -f $JANCOX_BIN $MODPATH/system/bin/jancox
rm -rf $MODPATH/bin
# Magisk/KernelSU/APatch: root:shell 0755 with the system_file label, so
# apps like Termux may run it. The Kopi installer uses permissions.sh.
if type set_perm >/dev/null 2>&1; then
	set_perm $MODPATH/system/bin/jancox 0 2000 0755 u:object_r:system_file:s0
else
	chmod 755 $MODPATH/system/bin/jancox
fi

# Check that the binary runs on this device (only when booted: in recovery
# the system libraries may not be mounted)
if $BOOTMODE; then
	JANCOX_VERSION=`$MODPATH/system/bin/jancox --version 2>/dev/null`
	if [ -n "$JANCOX_VERSION" ]; then
		print "- $JANCOX_VERSION works on this device"
	else
		print "[!] jancox did not run on this device; please report it in the Telegram group"
	fi
fi

# Termux puts its own bin first in PATH, so replace an old jancox there too.
# Give the copy Termux's owner and SELinux label, or Termux can't run it.
TERMUX_BIN=/data/data/com.termux/files/usr/bin
if [ -d $TERMUX_BIN ]; then
	print "- Termux detected, installing jancox in Termux"
	cp -f $MODPATH/system/bin/jancox $TERMUX_BIN/jancox
	chmod 755 $TERMUX_BIN/jancox
	TERMUX_OWNER=`stat -c %u:%g $TERMUX_BIN 2>/dev/null`
	TERMUX_LABEL=`stat -c %C $TERMUX_BIN 2>/dev/null`
	[ -n "$TERMUX_OWNER" ] && chown $TERMUX_OWNER $TERMUX_BIN/jancox 2>/dev/null
	[ -n "$TERMUX_LABEL" ] && chcon $TERMUX_LABEL $TERMUX_BIN/jancox 2>/dev/null
fi

# Jancox 2.5 kept its scripts and work files here; they may hold a ROM, so
# only point them out
if [ -d /data/local/jancox-tool ]; then
	print "- Old Jancox 2.5 files are still in /data/local/jancox-tool"
	print "  (not used anymore; delete them yourself when you don't need them)"
fi

print " "
print " How to use? Open a terminal (Termux), go to any folder and run:"
print "   jancox unpack    (reads the ROM zip from ./input/)"
print "   jancox repack    (writes the new ROM to ./output/)"
print " More: jancox --help"
print " "
