//! The CPU of the UDC interrupts.

/// The CPU that serves the interrupts of the UDC. The drop-in
/// `debian/reserve-cpu.conf` of `system.slice` and `user.slice` keeps all
/// services and sessions off it. The dwc2 UDC starts each isochronous transfer
/// from its interrupt, and an interrupt later than one microframe (125 µs) cuts
/// a video frame.
pub(crate) const CPU: u32 = 3;

/// Returns the numbers of the interrupts of `device` in `interrupts`, the text
/// of `/proc/interrupts`.
pub(crate) fn of_device(interrupts: &str, device: &str) -> Vec<u32> {
    interrupts
        .lines()
        .filter_map(|line| {
            let (number, rest) = line.split_once(':')?;
            let number = number.trim().parse().ok()?;
            rest.split([' ', ','])
                .any(|word| word == device)
                .then_some(number)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines of `/proc/interrupts` on a Raspberry Pi 5 with the gadget.
    const INTERRUPTS: &str = "\
           CPU0       CPU1       CPU2       CPU3
 13:   11624736   53063171   23385350   63006365    GICv2  26 Level     arch_timer
 33:  195067811          0          0          0    GICv2 105 Level     1000480000.usb, 1000480000.usb
136:   58605742          0          0          0 rp1_irq_chip  36 Edge      xhci-hcd:usb3
IPI0:     77947    8674359    6718785   11856144       Rescheduling interrupts
";

    #[test]
    fn interrupt_with_the_device_name_is_found() {
        let numbers = of_device(INTERRUPTS, "1000480000.usb");

        assert_eq!(numbers, [33]);
    }

    #[test]
    fn device_without_an_interrupt_has_none() {
        let numbers = of_device(INTERRUPTS, "1000490000.usb");

        assert_eq!(numbers, []);
    }

    #[test]
    fn part_of_a_device_name_does_not_match() {
        let numbers = of_device(INTERRUPTS, "xhci-hcd");

        assert_eq!(numbers, []);
    }

    #[test]
    fn slice_drop_in_leaves_out_the_interrupt_cpu() {
        let allowed = include_str!("../debian/reserve-cpu.conf")
            .lines()
            .find_map(|line| line.strip_prefix("AllowedCPUs="))
            .unwrap();

        let has_cpu = allowed.split([',', ' ']).any(|range| {
            let (first, last) = range.split_once('-').unwrap_or((range, range));
            (first.parse().unwrap()..=last.parse().unwrap()).contains(&CPU)
        });

        assert!(!has_cpu, "AllowedCPUs={allowed} has CPU {CPU}");
    }
}
