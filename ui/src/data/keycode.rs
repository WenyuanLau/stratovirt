// Copyright (c) 2022 Huawei Technologies Co.,Ltd. All rights reserved.
//
// StratoVirt is licensed under Mulan PSL v2.
// You can use this software according to the terms and conditions of the Mulan
// PSL v2.
// You may obtain a copy of Mulan PSL v2 at:
//         http://license.coscl.org.cn/MulanPSL2
// THIS SOFTWARE IS PROVIDED ON AN "AS IS" BASIS, WITHOUT WARRANTIES OF ANY
// KIND, EITHER EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO
// NON-INFRINGEMENT, MERCHANTABILITY OR FIT FOR A PARTICULAR PURPOSE.
// See the Mulan PSL v2 for more details.

pub const KEYSYM2KEYCODE: [(u16, u16); 173] = [
    // (Keysym , Keycode)
    (0x0020, 0x0039),
    (0x0021, 0x0102),
    (0x0022, 0x0128),
    (0x0023, 0x0104),
    (0x0024, 0x0105),
    (0x0025, 0x0106),
    (0x0026, 0x0108),
    (0x0027, 0x0028),
    (0x0028, 0x010A),
    (0x0029, 0x010B),
    (0x002A, 0x0109),
    (0x002B, 0x010D),
    (0x002C, 0x0033),
    (0x002D, 0x000C),
    (0x002E, 0x0034),
    (0x002F, 0x0035),
    (0x0030, 0x000B),
    (0x0031, 0x0002),
    (0x0032, 0x0003),
    (0x0033, 0x0004),
    (0x0034, 0x0005),
    (0x0035, 0x0006),
    (0x0036, 0x0007),
    (0x0037, 0x0008),
    (0x0038, 0x0009),
    (0x0039, 0x000A),
    (0x003A, 0x0127),
    (0x003B, 0x0027),
    (0x003C, 0x0133),
    (0x003D, 0x000D),
    (0x003E, 0x0134),
    (0x003F, 0x0135),
    (0x0040, 0x0103),
    (0x0041, 0x011E),
    (0x0042, 0x0130),
    (0x0043, 0x012E),
    (0x0044, 0x0120),
    (0x0045, 0x0112),
    (0x0046, 0x0121),
    (0x0047, 0x0122),
    (0x0048, 0x0123),
    (0x0049, 0x0117),
    (0x004A, 0x0124),
    (0x004B, 0x0125),
    (0x004C, 0x0126),
    (0x004D, 0x0132),
    (0x004E, 0x0131),
    (0x004F, 0x0118),
    (0x0050, 0x0119),
    (0x0051, 0x0110),
    (0x0052, 0x0113),
    (0x0053, 0x011F),
    (0x0054, 0x0114),
    (0x0055, 0x0116),
    (0x0056, 0x012F),
    (0x0057, 0x0111),
    (0x0058, 0x012D),
    (0x0059, 0x0115),
    (0x005A, 0x012C),
    (0x005B, 0x001A),
    (0x005C, 0x002B),
    (0x005D, 0x001B),
    (0x005E, 0x0107),
    (0x005F, 0x010C),
    (0x0060, 0x0029),
    (0x0061, 0x001E),
    (0x0062, 0x0030),
    (0x0063, 0x002E),
    (0x0064, 0x0020),
    (0x0065, 0x0012),
    (0x0066, 0x0021),
    (0x0067, 0x0022),
    (0x0068, 0x0023),
    (0x0069, 0x0017),
    (0x006A, 0x0024),
    (0x006B, 0x0025),
    (0x006C, 0x0026),
    (0x006D, 0x0032),
    (0x006E, 0x0031),
    (0x006F, 0x0018),
    (0x0070, 0x0019),
    (0x0071, 0x0010),
    (0x0072, 0x0013),
    (0x0073, 0x001F),
    (0x0074, 0x0014),
    (0x0075, 0x0016),
    (0x0076, 0x002F),
    (0x0077, 0x0011),
    (0x0078, 0x002D),
    (0x0079, 0x0015),
    (0x007A, 0x002C),
    (0x007B, 0x011A),
    (0x007C, 0x012B),
    (0x007D, 0x011B),
    (0x007E, 0x0129),
    (0x00A6, 0x0956),
    (0xFE03, 0x00B8),
    (0xFF08, 0x000E),
    (0xFF09, 0x000F),
    (0xFF0D, 0x001C),
    (0xFF13, 0x00C6),
    (0xFF14, 0x0046),
    (0xFF15, 0x0054),
    (0xFF1B, 0x0001),
    (0xFF22, 0x007B),
    (0xFF23, 0x0079),
    (0xFF50, 0x00C7),
    (0xFF51, 0x00CB),
    (0xFF52, 0x00C8),
    (0xFF53, 0x00CD),
    (0xFF54, 0x00D0),
    (0xFF55, 0x00C9),
    (0xFF56, 0x00D1),
    (0xFF57, 0x00CF),
    (0xFF61, 0x0054),
    (0xFF62, 0x0054),
    (0xFF63, 0x00D2),
    (0xFF67, 0x00DD),
    (0xFF7E, 0x00B8),
    (0xFF7F, 0x0045),
    (0xFF8D, 0x009C),
    (0xFF95, 0x0047),
    (0xFF96, 0x004B),
    (0xFF97, 0x0048),
    (0xFF98, 0x004D),
    (0xFF99, 0x0050),
    (0xFF9A, 0x0049),
    (0xFF9B, 0x0051),
    (0xFF9C, 0x004F),
    (0xFF9D, 0x004C),
    (0xFF9E, 0x0052),
    (0xFF9F, 0x0053),
    (0xFFAA, 0x0037),
    (0xFFAB, 0x004E),
    (0xFFAC, 0x0053),
    (0xFFAD, 0x004A),
    (0xFFAE, 0x0053),
    (0xFFAF, 0x00B5),
    (0xFFB0, 0x0052),
    (0xFFB1, 0x004F),
    (0xFFB2, 0x0050),
    (0xFFB3, 0x0051),
    (0xFFB4, 0x004B),
    (0xFFB5, 0x004C),
    (0xFFB6, 0x004D),
    (0xFFB7, 0x0047),
    (0xFFB8, 0x0048),
    (0xFFB9, 0x0049),
    (0xFFBD, 0x0059),
    (0xFFBE, 0x003B),
    (0xFFBF, 0x003C),
    (0xFFC0, 0x003D),
    (0xFFC1, 0x003E),
    (0xFFC2, 0x003F),
    (0xFFC3, 0x0040),
    (0xFFC4, 0x0041),
    (0xFFC5, 0x0042),
    (0xFFC6, 0x0043),
    (0xFFC7, 0x0044),
    (0xFFC8, 0x0057),
    (0xFFC9, 0x0058),
    (0xFFE1, 0x002A),
    (0xFFE2, 0x0036),
    (0xFFE3, 0x001D),
    (0xFFE4, 0x009D),
    (0xFFE5, 0x003A),
    (0xFFE7, 0x0138),
    (0xFFE8, 0x01B8),
    (0xFFE9, 0x0038),
    (0xFFEA, 0x00B8),
    (0xFFEB, 0x00DB),
    (0xFFEC, 0x00DC),
    (0xFFFF, 0x00D3),
];