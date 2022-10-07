use std::{io, time::Duration};

use calloop::{
    channel::{self, Channel},
    timer::{TimeoutAction, Timer},
    EventSource, Poll, PostAction, Readiness, Token, TokenFactory,
};
use wayland_client::{
    protocol::{wl_keyboard, wl_seat},
    Dispatch, QueueHandle,
};

use super::{
    KeyEvent, KeyboardData, KeyboardDataExt, KeyboardError, KeyboardHandler, RepeatInfo, RMLVO,
};
use crate::seat::SeatState;

/// Internal repeat message sent to the repeating mechanism.
#[derive(Debug)]
pub(crate) enum RepeatMessage {
    StopRepeat,

    /// The key event should not have any time added, the repeating mechanism is responsible for that instead.
    StartRepeat(KeyEvent),

    RepeatInfo(RepeatInfo),
}

/// [`EventSource`] used to emit key repeat events.
#[derive(Debug)]
pub struct KeyRepeatSource {
    channel: Channel<RepeatMessage>,
    timer: Timer,
    /// Gap in time to the next key event in milliseconds.
    gap: u64,
    delay: u64,
    disabled: bool,
    key: Option<KeyEvent>,
}

impl SeatState {
    /// Creates a keyboard from a seat.
    ///
    /// This function returns an [`EventSource`] that indicates when a key press is going to repeat.
    ///
    /// This keyboard implementation uses libxkbcommon for the keymap.
    ///
    /// Typically the compositor will provide a keymap, but you may specify your own keymap using the `rmlvo`
    /// field.
    ///
    /// ## Errors
    ///
    /// This will return [`SeatError::UnsupportedCapability`] if the seat does not support a keyboard.
    pub fn get_keyboard_with_repeat<D>(
        &mut self,
        qh: &QueueHandle<D>,
        seat: &wl_seat::WlSeat,
        rmlvo: Option<RMLVO>,
    ) -> Result<(wl_keyboard::WlKeyboard, KeyRepeatSource), KeyboardError>
    where
        D: Dispatch<wl_keyboard::WlKeyboard, KeyboardData> + KeyboardHandler + 'static,
    {
        let udata = match rmlvo {
            Some(rmlvo) => KeyboardData::from_rmlvo(rmlvo)?,
            None => KeyboardData::default(),
        };

        self.get_keyboard_with_repeat_with_data(qh, seat, udata)
    }

    /// Creates a keyboard from a seat.
    ///
    /// This function returns an [`EventSource`] that indicates when a key press is going to repeat.
    ///
    /// This keyboard implementation uses libxkbcommon for the keymap.
    ///
    /// Typically the compositor will provide a keymap, but you may specify your own keymap using the `rmlvo`
    /// field.
    ///
    /// ## Errors
    ///
    /// This will return [`SeatError::UnsupportedCapability`] if the seat does not support a keyboard.
    pub fn get_keyboard_with_repeat_with_data<D, U>(
        &mut self,
        qh: &QueueHandle<D>,
        seat: &wl_seat::WlSeat,
        mut udata: U,
    ) -> Result<(wl_keyboard::WlKeyboard, KeyRepeatSource), KeyboardError>
    where
        D: Dispatch<wl_keyboard::WlKeyboard, U> + KeyboardHandler + 'static,
        U: KeyboardDataExt + 'static,
    {
        let (repeat_sender, channel) = channel::channel();

        let kbd_data = udata.keyboard_data_mut();
        kbd_data.repeat_sender.replace(repeat_sender);
        kbd_data.init_compose();

        let repeat = KeyRepeatSource {
            channel,
            timer: Timer::immediate(),
            gap: 0,
            delay: 0,
            key: None,
            disabled: true,
        };

        Ok((seat.get_keyboard(qh, udata), repeat))
    }
}

impl EventSource for KeyRepeatSource {
    type Event = KeyEvent;
    type Metadata = ();
    type Ret = ();
    type Error = io::Error;

    fn process_events<F>(
        &mut self,
        readiness: Readiness,
        token: Token,
        mut callback: F,
    ) -> io::Result<PostAction>
    where
        F: FnMut(Self::Event, &mut Self::Metadata) -> Self::Ret,
    {
        let mut removed = false;

        let timer = &mut self.timer;
        let gap = &mut self.gap;
        let delay_mut = &mut self.delay;
        let key = &mut self.key;

        // Check if the key repeat should stop
        self.channel
            .process_events(readiness, token, |event, _| {
                match event {
                    channel::Event::Msg(message) => {
                        match message {
                            RepeatMessage::StopRepeat => {
                                key.take();
                            }

                            RepeatMessage::StartRepeat(mut event) => {
                                // Update time for next event
                                event.time += *delay_mut as u32;
                                key.replace(event);

                                // Schedule a new press event in the timer.
                                timer.set_duration(Duration::from_millis(*delay_mut));
                            }

                            RepeatMessage::RepeatInfo(info) => {
                                match info {
                                    // Store the repeat time, using it for the next repeat sequence.
                                    RepeatInfo::Repeat { rate, delay } => {
                                        // Number of repetitions per second / 1000 ms
                                        *gap = (rate.get() / 1000) as u64;
                                        *delay_mut = delay as u64;
                                        self.disabled = false;
                                        timer.set_duration(Duration::from_millis(*delay_mut));
                                    }

                                    RepeatInfo::Disable => {
                                        // Compositor will send repeat events manually, cancel all repeating events
                                        key.take();
                                        self.disabled = true;
                                    }
                                }
                            }
                        }
                    }

                    channel::Event::Closed => {
                        removed = true;
                    }
                }
            })
            .unwrap();

        // Keyboard was destroyed
        if removed {
            return Ok(PostAction::Remove);
        }

        timer.process_events(readiness, token, |mut event, _| {
            if self.disabled || key.is_none() {
                // TODO How to pause the timer without dropping it?
                return TimeoutAction::ToDuration(Duration::from_millis(*delay_mut));
            }
            // Invoke the event
            callback(key.clone().unwrap(), &mut ());

            // Update time for next event
            event += Duration::from_millis(*gap);
            // Schedule the next key press
            TimeoutAction::ToDuration(Duration::from_micros(*gap))
        })
    }

    fn register(
        &mut self,
        poll: &mut Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        self.channel.register(poll, token_factory)?;
        self.timer.register(poll, token_factory)
    }

    fn reregister(
        &mut self,
        poll: &mut Poll,
        token_factory: &mut TokenFactory,
    ) -> calloop::Result<()> {
        self.channel.reregister(poll, token_factory)?;
        self.timer.reregister(poll, token_factory)
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.channel.unregister(poll)?;
        self.timer.unregister(poll)
    }
}
