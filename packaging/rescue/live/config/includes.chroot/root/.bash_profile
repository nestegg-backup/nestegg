# nestegg rescue system: start the restore wizard on the first console.
[ -f ~/.bashrc ] && . ~/.bashrc
if [ "$(tty)" = /dev/tty1 ] && [ -z "$NESTEGG_WIZARD_DONE" ]; then
  export NESTEGG_WIZARD_DONE=1
  /usr/local/bin/nestegg rescue
  echo
  echo "Shell of the rescue system. Start the wizard again with: nestegg rescue"
  echo "Wi-Fi: nmtui    Disks: nestegg rescue disks    Restart: reboot"
fi
