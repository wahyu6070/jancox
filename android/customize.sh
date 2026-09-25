
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

# ARCH is set by Magisk and by the Kopi installer: arm, arm64, x86 or x64
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
chmod 755 $MODPATH/system/bin/jancox
rm -rf $MODPATH/bin

# Termux puts its own bin first in PATH, so replace an old jancox there too
if [ -d /data/data/com.termux/files/usr/bin ]; then
	print "- Termux detected installing jancox in termux"
	cp -f $MODPATH/system/bin/jancox /data/data/com.termux/files/usr/bin/jancox
	chmod 755 /data/data/com.termux/files/usr/bin/jancox
fi

print " "
print " How to use? "
print " Open terminal"
print " jancox --help"
print " "
